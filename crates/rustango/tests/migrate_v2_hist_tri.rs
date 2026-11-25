//! Engine v2 historical tables (5.3, 5.5) through a step context, on
//! every backend: the 14 types, strict decode, coercion, auto keys,
//! upsert/insert_ignore/insert_many/exists/raw, interning, and one
//! statement per write on an audited generic-M2M owner.

#![cfg(all(
    feature = "testkit",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use rustango::audit::{self, AuditLog};
use rustango::core::{
    CountQuery, FieldSchema, FieldType, Model as _, ModelEntry, ModelSchema, SelectQuery, SqlValue,
};
use rustango::migrate::v2::testing::{end, session};
use rustango::migrate::v2::{leaked_bytes, HistError, HistTable, StepCtx};
use rustango::migrate::{detect_changes, render_changes_split_with_dialect};
use rustango::migrate::{SchemaSnapshot, TableSnapshot};
use rustango::sql::{CounterPool as _, Pool};
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::{tri_dialect_test, Model};
use serde_json::json;

/// An audited owner with a generic M2M: every shared delete/update path
/// would add junction cleanup or audit rows for it.
#[derive(Model, Debug, Clone)]
#[rustango(table = "s1_gaudited", app = "s1", audit(track = "label"))]
#[rustango(generic_m2m(
    name = "tags",
    through = "s1_taggables",
    pk_column = "taggable_id",
    ct_column = "taggable_type",
    related_column = "tag_id"
))]
pub struct S1Gaudited {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 16)]
    pub label: String,
}

const TAGGABLES: &ModelSchema = &{
    const FIELDS: &[FieldSchema] = &[
        FieldSchema::new("taggable_id", "taggable_id", FieldType::I64),
        FieldSchema::new("taggable_type", "taggable_type", FieldType::I64),
        FieldSchema::new("tag_id", "tag_id", FieldType::I64),
    ];
    let mut s = ModelSchema::new("S1Taggable", "s1_taggables");
    s.fields = FIELDS;
    s
};

const TABLES: &[&str] = &["s1_kinds", "s1_points", "s1_bad", "s1_up"];

fn snap(v: serde_json::Value) -> TableSnapshot {
    serde_json::from_value(v).expect("snapshot json")
}

fn f(column: &str, ty: &str) -> serde_json::Value {
    json!({"name": column, "column": column, "ty": ty, "nullable": false, "primary_key": false})
}

fn with(mut v: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

/// Every portable type, an auto pk and two nullable columns.
fn kinds() -> TableSnapshot {
    snap(json!({"name": "s1_kinds", "model": "Kinds", "fields": [
        with(f("id", "i64"), json!({"primary_key": true, "auto": true})),
        f("c_i16", "i16"), f("c_i32", "i32"), f("c_i64", "i64"), f("c_f64", "f64"),
        with(f("c_str", "string"), json!({"max_length": 64})),
        f("c_bool", "bool"), f("c_dec", "decimal"), f("c_uuid", "uuid"), f("c_json", "json"),
        f("c_date", "date"), f("c_time", "time"), f("c_dt", "datetime"), f("c_bin", "binary"),
        with(f("c_null_str", "string"), json!({"max_length": 64, "nullable": true})),
        with(f("c_null_i32", "i32"), json!({"nullable": true})),
    ]}))
}

/// Create/alter through the migrate renderer, as a v2 runner would.
async fn apply(pool: &Pool, old: &[TableSnapshot], new: &[TableSnapshot]) -> Vec<String> {
    let old = SchemaSnapshot {
        tables: old.to_vec(),
        ..Default::default()
    };
    let new = SchemaSnapshot {
        tables: new.to_vec(),
        ..Default::default()
    };
    let changes = detect_changes(&old, &new);
    let batch = render_changes_split_with_dialect(&changes, &new, pool.dialect()).expect("render");
    assert!(batch.rebuild.is_none(), "spike tables never rebuild");
    for sql in &batch.immediate {
        rustango::sql::raw_execute_pool(pool, sql, Vec::new())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    batch.immediate
}

async fn setup(pool: &Pool) {
    for t in TABLES {
        drop_table(pool, t).await;
    }
    drop_table(pool, TAGGABLES.table).await;
    fresh_table::<S1Gaudited>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    rustango::testkit::create_tables(pool, &[TAGGABLES])
        .await
        .expect("taggables");
}

fn dt(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().into()
}
fn uuid_n(n: u8) -> uuid::Uuid {
    uuid::Uuid::from_bytes([n; 16])
}
fn dec(s: &str) -> rust_decimal::Decimal {
    s.parse().unwrap()
}

/// Row `n` (1..=3): each ordered column grows with `n`; row 3 has NULLs.
fn kinds_row(n: u8) -> Vec<(&'static str, SqlValue)> {
    let i = i16::from(n);
    let names = ["alpha", "beta", "gamma"];
    vec![
        ("c_i16", SqlValue::I16(i)),
        ("c_i32", SqlValue::I32(i32::from(i) * 1000)),
        ("c_i64", SqlValue::I64(i64::from(i) * 10_000_000_000)),
        ("c_f64", SqlValue::F64(f64::from(i) + 0.5)),
        ("c_str", SqlValue::String(names[usize::from(n - 1)].into())),
        ("c_bool", SqlValue::Bool(n % 2 == 1)),
        ("c_dec", SqlValue::Decimal(dec(&format!("{n}2.34")))),
        ("c_uuid", SqlValue::Uuid(uuid_n(n))),
        ("c_json", SqlValue::Json(json!({"n": n, "l": [true, null]}))),
        (
            "c_date",
            SqlValue::Date(chrono::NaiveDate::from_ymd_opt(2026, 1, u32::from(n)).unwrap()),
        ),
        (
            "c_time",
            SqlValue::Time(
                chrono::NaiveTime::from_hms_micro_opt(10, u32::from(n), 12, 500_000).unwrap(),
            ),
        ),
        (
            "c_dt",
            SqlValue::DateTime(dt(&format!("2026-01-0{n}T03:04:05.123456Z"))),
        ),
        ("c_bin", SqlValue::Binary(vec![0, n, 255])),
        (
            "c_null_str",
            if n == 3 {
                SqlValue::Null
            } else {
                SqlValue::String(format!("n{n}"))
            },
        ),
        (
            "c_null_i32",
            if n == 3 {
                SqlValue::Null
            } else {
                SqlValue::I32(i32::from(n))
            },
        ),
    ]
}

fn backend(pool: &Pool) -> &'static str {
    pool.dialect().name()
}

async fn ctx_on(pool: &Pool, snap: &TableSnapshot) -> (StepCtx<'static>, HistTable) {
    let ctx = session(pool, "s1", std::slice::from_ref(snap))
        .await
        .unwrap();
    let t = ctx.table(&snap.name).unwrap();
    (ctx, t)
}

/// Every type round-trips and filters, on each backend.
async fn hist_types_round_trip(pool: &Pool) {
    apply(pool, &[], &[kinds()]).await;
    let (mut ctx, t) = ctx_on(pool, &kinds()).await;
    for n in 1..=3 {
        assert_eq!(t.insert(&mut ctx, &kinds_row(n)).await.unwrap(), 1);
    }
    let rows = t
        .query()
        .order_by("id", false)
        .fetch(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    for (n, row) in (1..=3u8).zip(&rows) {
        for (col, want) in kinds_row(n) {
            let got = row.get(col).unwrap();
            assert_eq!(got, &want, "{} row {n} column {col}", backend(pool));
        }
    }
    for (col, v) in kinds_row(2) {
        let n = t.query().filter_eq(col, v.clone()).count(&mut ctx).await;
        let n = n.unwrap_or_else(|e| panic!("{} eq {col}: {e}", backend(pool)));
        if col != "c_json" {
            assert_eq!(n, 1, "{} eq {col}", backend(pool));
        }
    }
    assert!(t
        .query()
        .filter_is_null("c_null_i32", true)
        .exists(&mut ctx)
        .await
        .unwrap());
    end(ctx, true).await.unwrap();
}

/// Lossless widening only; auto keys; upsert, insert_ignore, insert_many,
/// exists and raw.
async fn hist_writes(pool: &Pool) {
    let up = snap(json!({"name": "s1_up", "model": "Up", "fields": [
        with(f("id", "i64"), json!({"primary_key": true, "auto": true})),
        with(f("code", "string"), json!({"max_length": 16, "unique": true})),
        f("n", "i32"),
    ]}));
    apply(pool, &[], &[up.clone()]).await;
    let (mut ctx, t) = ctx_on(pool, &up).await;
    // i16 widens into i32; i64 does not narrow.
    t.insert(&mut ctx, &[("code", "a".into()), ("n", SqlValue::I16(1))])
        .await
        .unwrap();
    let e = t
        .insert(&mut ctx, &[("code", "b".into()), ("n", SqlValue::I64(1))])
        .await;
    assert!(matches!(e, Err(HistError::Coerce { .. })), "{e:?}");
    // An explicit auto key needs reset_sequence.
    let e = t
        .insert(
            &mut ctx,
            &[
                ("id", 5000_i64.into()),
                ("code", "c".into()),
                ("n", SqlValue::I32(1)),
            ],
        )
        .await;
    assert!(matches!(e, Err(HistError::ExplicitAuto { .. })), "{e:?}");
    t.clone()
        .reset_sequence()
        .insert(
            &mut ctx,
            &[
                ("id", 5000_i64.into()),
                ("code", "c".into()),
                ("n", SqlValue::I32(1)),
            ],
        )
        .await
        .unwrap();
    let one = |c: &str, n: i32| vec![("code", SqlValue::from(c)), ("n", SqlValue::I32(n))];
    // The duplicate is skipped (MySQL reports a no-op write as 1 row).
    t.insert_ignore(&mut ctx, &one("a", 9)).await.unwrap();
    assert!(t
        .query()
        .filter_eq("code", "a")
        .filter_eq("n", 1)
        .exists(&mut ctx)
        .await
        .unwrap());
    t.upsert(&mut ctx, &["code"], &one("a", 7)).await.unwrap();
    let many: Vec<_> = (0..300).map(|i| one(&format!("m{i}"), i)).collect();
    assert_eq!(t.insert_many(&mut ctx, &many).await.unwrap(), 300);
    assert!(t
        .query()
        .filter_eq("code", "a")
        .filter_eq("n", 7)
        .exists(&mut ctx)
        .await
        .unwrap());
    let rows = ctx
        .raw(
            "SELECT {{c:n}} FROM {{t:s1_up}} WHERE {{c:code}} = {{?}}",
            vec!["a".into()],
            Some(("s1_up", &["n"])),
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get("n"), Some(&SqlValue::I32(7)));
    end(ctx, true).await.unwrap();
    // The unit's end moved the PG sequence past the explicit key.
    let (mut ctx, t) = ctx_on(pool, &up).await;
    t.insert(
        &mut ctx,
        &[("code", "after".into()), ("n", SqlValue::I32(1))],
    )
    .await
    .unwrap();
    let after = t
        .query()
        .filter_eq("code", "after")
        .fetch(&mut ctx)
        .await
        .unwrap();
    end(ctx, true).await.unwrap();
    if backend(pool) == "postgres" {
        assert_eq!(after[0].get("id"), Some(&SqlValue::I64(5001)));
    }
}

async fn taggable_rows(pool: &Pool) -> i64 {
    rustango::sql::count_rows_pool(pool, &CountQuery::from_select(SelectQuery::new(TAGGABLES)))
        .await
        .unwrap()
}

async fn audit_rows(pool: &Pool) -> i64 {
    AuditLog::objects()
        .filter("entity_table", "s1_gaudited")
        .count(pool)
        .await
        .unwrap()
}

/// On an audited generic-M2M owner, a historical delete and update are one
/// statement each, add no audit row and leave the junction alone.
async fn hist_writes_on_an_audited_generic_m2m_owner(pool: &Pool) {
    rustango::contenttypes::ensure_seeded(pool).await.unwrap();
    rustango::contenttypes::clear_cache();
    for id in 1..=2 {
        let mut a = S1Gaudited {
            id,
            label: "a".into(),
        };
        a.insert(pool).await.unwrap();
        a.tags_m2m().set(&[id, 10], pool).await.unwrap();
    }
    let (links, audits) = (taggable_rows(pool).await, audit_rows(pool).await);
    let state = SchemaSnapshot::from_models_forced(&[S1Gaudited::SCHEMA]);
    let snap = state
        .tables
        .iter()
        .find(|t| t.name == "s1_gaudited")
        .unwrap();
    let (mut ctx, t) = ctx_on(pool, snap).await;
    assert!(
        t.schema().name.starts_with("__hist:"),
        "{}",
        t.schema().name
    );
    assert!(ModelEntry::for_schema(t.schema()).is_none());
    let stmt = t
        .query()
        .filter_eq("id", 1_i64)
        .compile_delete(pool.dialect())
        .unwrap();
    assert!(stmt.sql.starts_with("DELETE FROM"), "{}", stmt.sql);
    assert!(!stmt.sql.contains("s1_taggables"), "{}", stmt.sql);
    assert_eq!(
        t.query()
            .filter_eq("id", 1_i64)
            .delete(&mut ctx)
            .await
            .unwrap(),
        1
    );
    t.query()
        .filter_eq("id", 2_i64)
        .update(&mut ctx, &[("label", "b".into())])
        .await
        .unwrap();
    end(ctx, true).await.unwrap();
    assert_eq!(
        taggable_rows(pool).await,
        links,
        "hist delete touched the junction"
    );
    assert_eq!(audit_rows(pool).await, audits, "hist write was audited");
}

/// A historical delete of an audited model's row is one statement and
/// writes no audit row.
async fn hist_delete_of_an_audited_row_is_not_audited(pool: &Pool) {
    let audits = audit_rows(pool).await;
    let state = SchemaSnapshot::from_models_forced(&[S1Gaudited::SCHEMA]);
    let snap = state
        .tables
        .iter()
        .find(|t| t.name == "s1_gaudited")
        .unwrap();
    let mut a = S1Gaudited {
        id: 7,
        label: "p".into(),
    };
    a.insert(pool).await.unwrap();
    let (mut ctx, t) = ctx_on(pool, snap).await;
    assert_eq!(
        t.query()
            .filter_eq("id", 7_i64)
            .delete(&mut ctx)
            .await
            .unwrap(),
        1
    );
    end(ctx, true).await.unwrap();
    assert_eq!(
        audit_rows(pool).await,
        audits + 1,
        "only the model insert was audited"
    );
}

/// A cell that does not read as its state type is an error.
async fn hist_strict_decode_fails_on_a_bad_cell(pool: &Pool) {
    let wide = snap(json!({"name": "s1_bad", "model": "Bad", "fields": [
        with(f("id", "i64"), json!({"primary_key": true})), f("n", "i64"),
    ]}));
    let narrow = snap(json!({"name": "s1_bad", "model": "Bad", "fields": [
        with(f("id", "i64"), json!({"primary_key": true})), f("n", "i16"),
    ]}));
    apply(pool, &[], &[wide.clone()]).await;
    let (mut ctx, w) = ctx_on(pool, &wide).await;
    w.insert(&mut ctx, &[("id", 1_i64.into()), ("n", 100_000_i64.into())])
        .await
        .unwrap();
    end(ctx, true).await.unwrap();
    let (mut ctx, n) = ctx_on(pool, &narrow).await;
    let got = n.query().fetch(&mut ctx).await;
    assert!(got.is_err(), "{}: silent decode {got:?}", backend(pool));
    end(ctx, false).await.unwrap();
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        hist_types_round_trip,
        hist_writes,
        hist_writes_on_an_audited_generic_m2m_owner,
        hist_delete_of_an_audited_row_is_not_audited,
        hist_strict_decode_fails_on_a_bad_cell,
    ]
}

/// Leaked bytes per distinct state, and none on re-intern.
#[tokio::test]
async fn hist_interning_leaks_once_per_state() {
    let Ok(pool) = Pool::connect("sqlite::memory:").await else {
        return;
    };
    let mut k = kinds();
    k.name = "s1_leak_kinds".into();
    let before = leaked_bytes();
    let ctx = session(&pool, "s1", &[k.clone()]).await.unwrap();
    let a = ctx.table("s1_leak_kinds").unwrap();
    let once = leaked_bytes() - before;
    assert!(once > 0);
    let mut shuffled = k.clone();
    shuffled.fields.reverse();
    shuffled.model = "Renamed".into();
    for _ in 0..100 {
        let ctx = session(&pool, "s1", &[shuffled.clone()]).await.unwrap();
        let again = ctx.table("s1_leak_kinds").unwrap();
        assert!(std::ptr::eq(a.schema(), again.schema()));
    }
    assert_eq!(leaked_bytes() - before, once, "re-interning leaked");
}
