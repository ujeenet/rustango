//! SPIKE S1 (migrations engine v2): the historical ORM over a leaked
//! runtime schema, on every backend. Throwaway.

#![cfg(all(
    feature = "migrate-v2-spike",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use rustango::audit::{self, AuditLog};
use rustango::core::{
    Assignment, CountQuery, DeleteQuery, FieldSchema, FieldType, Filter, Model as _, ModelEntry,
    ModelSchema, Op, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};
use rustango::migrate::v2_spike::hist::{leaked_bytes, HistError, HistTable, HIST_PREFIX};
use rustango::migrate::{detect_changes, render_changes_split_with_dialect};
use rustango::migrate::{SchemaSnapshot, TableSnapshot};
use rustango::sql::{CounterPool as _, Pool};
use rustango::test_assertions::query_counter::assert_num_queries;
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

const TABLES: &[&str] = &["s1_kinds", "s1_points", "s1_tenant", "s1_bad"];

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

fn points(added: bool) -> TableSnapshot {
    let mut fields = vec![
        with(f("id", "i64"), json!({"primary_key": true})),
        with(f("title", "string"), json!({"max_length": 32})),
    ];
    if added {
        fields.push(with(
            f("note", "string"),
            json!({"max_length": 32, "nullable": true}),
        ));
    }
    snap(json!({"name": "s1_points", "model": "Point", "fields": fields}))
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

/// Criterion 1: typed CRUD and filters for every type, on each backend.
async fn hist_types_round_trip_and_filters(pool: &Pool) {
    let ddl = apply(pool, &[], &[kinds()]).await.join("\n");
    if backend(pool) == "mysql" {
        assert!(ddl.contains("CHAR(36)"), "mysql uuid column: {ddl}");
        assert!(ddl.contains("TINYINT(1)"), "mysql bool column: {ddl}");
    }
    let t = HistTable::from_snapshot("s1", &kinds()).unwrap();
    for n in 1..=3 {
        assert_eq!(t.insert(pool, &kinds_row(n)).await.unwrap(), 1);
    }

    // Round trip, strictly decoded.
    let rows = t.query().order_by("id", false).fetch(pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    for (n, row) in (1..=3).zip(&rows) {
        assert_eq!(row.get("id"), Some(&SqlValue::I64(i64::from(n))), "auto pk");
        for (col, want) in kinds_row(n) {
            let got = row.get(col).unwrap_or_else(|| panic!("no {col}"));
            assert_eq!(got, &want, "{} row {n} column {col}", backend(pool));
        }
    }

    // eq / ne on every non-null column.
    let row1 = kinds_row(1);
    for (col, v) in row1.iter().filter(|(c, _)| *c != "c_json") {
        let eq = t.query().filter_eq(col, v.clone()).count(pool).await;
        let eq = eq.unwrap_or_else(|e| panic!("{} eq {col}: {e}", backend(pool)));
        let ne = t
            .query()
            .filter_ne(col, v.clone())
            .count(pool)
            .await
            .unwrap();
        // bool: row 3 is also true; nullable: NULL never matches `<>`.
        let (want_eq, want_ne) = match *col {
            "c_bool" => (2, 1),
            "c_null_str" | "c_null_i32" => (1, 1),
            _ => (1, 2),
        };
        assert_eq!(
            (eq, ne),
            (want_eq, want_ne),
            "{} eq/ne {col}",
            backend(pool)
        );
    }

    // gt / lt on every ordered column.
    let row3 = kinds_row(3);
    for col in [
        "c_i16", "c_i32", "c_i64", "c_f64", "c_str", "c_dec", "c_date", "c_time", "c_dt",
    ] {
        let lo = row1.iter().find(|(c, _)| *c == col).unwrap().1.clone();
        let hi = row3.iter().find(|(c, _)| *c == col).unwrap().1.clone();
        let gt = t.query().filter_gt(col, lo).count(pool).await.unwrap();
        let lt = t.query().filter_lt(col, hi).count(pool).await.unwrap();
        assert_eq!((gt, lt), (2, 2), "{} gt/lt {col}", backend(pool));
    }

    // in / is_null / order_by + limit / widening.
    let ids = |rows: Vec<rustango::migrate::v2_spike::hist::HistRow>| -> Vec<SqlValue> {
        rows.iter().map(|r| r.get("id").unwrap().clone()).collect()
    };
    let n = t
        .query()
        .filter_in("c_uuid", vec![uuid_n(1).into(), uuid_n(3).into()])
        .count(pool)
        .await
        .unwrap();
    assert_eq!(n, 2, "in uuid");
    let n = t
        .query()
        .filter_in("c_i32", vec![SqlValue::I32(2000), SqlValue::I16(3)])
        .count(pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "in i32 with a widened i16");
    assert_eq!(
        t.query()
            .filter_is_null("c_null_str", true)
            .count(pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        t.query()
            .filter_is_null("c_null_i32", false)
            .count(pool)
            .await
            .unwrap(),
        2
    );
    let top = t
        .query()
        .order_by("c_i64", true)
        .limit(2)
        .fetch(pool)
        .await
        .unwrap();
    assert_eq!(ids(top), vec![SqlValue::I64(3), SqlValue::I64(2)]);
    assert_eq!(
        t.query()
            .filter_eq("c_i64", 7_i32)
            .count(pool)
            .await
            .unwrap(),
        0
    );

    // Refusals: narrowing, unknown column, missing NOT NULL value.
    let e = t
        .query()
        .filter_eq("c_i16", 7_i64)
        .count(pool)
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            HistError::Coerce {
                column: "c_i16",
                ..
            }
        ),
        "{e}"
    );
    let e = t
        .query()
        .filter_eq("nope", 1_i64)
        .count(pool)
        .await
        .unwrap_err();
    assert!(matches!(e, HistError::UnknownColumn { .. }), "{e}");
    let e = t
        .insert(pool, &[("c_i16", SqlValue::I16(1))])
        .await
        .unwrap_err();
    assert!(matches!(e, HistError::MissingValue { .. }), "{e}");

    // update + delete.
    let set = [
        ("c_str", SqlValue::String("zeta".into())),
        ("c_bool", SqlValue::Bool(false)),
        ("c_json", SqlValue::Json(json!({"u": 1}))),
        ("c_null_str", SqlValue::Null),
    ];
    let n = t
        .query()
        .filter_eq("c_uuid", uuid_n(1))
        .update(pool, &set)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let row = t.query().filter_eq("id", 1_i64).fetch(pool).await.unwrap();
    for (col, v) in &set {
        assert_eq!(row[0].get(col), Some(v), "updated {col}");
    }
    let n = t
        .query()
        .filter_in("id", vec![SqlValue::I64(1), SqlValue::I64(2)])
        .delete(pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(t.query().count(pool).await.unwrap(), 1);

    // SQLite: a datetime stored as legacy text still decodes.
    if backend(pool) == "sqlite" {
        rustango::sql::raw_execute_pool(
            pool,
            "UPDATE s1_kinds SET c_dt = '2026-01-09 03:04:05' WHERE id = 3",
            Vec::new(),
        )
        .await
        .unwrap();
        let row = t.query().filter_eq("id", 3_i64).fetch(pool).await.unwrap();
        assert_eq!(
            row[0].get("c_dt"),
            Some(&SqlValue::DateTime(dt("2026-01-09T03:04:05Z")))
        );
    }
}

/// JSON equality as a filter: probed apart, since its SQL differs per backend.
async fn hist_json_eq_filter(pool: &Pool) {
    apply(pool, &[], &[kinds()]).await;
    let t = HistTable::from_snapshot("s1", &kinds()).unwrap();
    t.insert(pool, &kinds_row(1)).await.unwrap();
    let v = kinds_row(1)
        .into_iter()
        .find(|(c, _)| *c == "c_json")
        .unwrap()
        .1;
    let n = t.query().filter_eq("c_json", v).count(pool).await;
    eprintln!("{}: json eq filter -> {n:?}", backend(pool));
}

/// Criterion 2: two state points of one table in one process.
async fn hist_two_state_points_one_table(pool: &Pool) {
    apply(pool, &[], &[points(false)]).await;
    let v1 = HistTable::from_snapshot("s1", &points(false)).unwrap();
    v1.insert(pool, &[("id", 1_i64.into()), ("title", "one".into())])
        .await
        .unwrap();
    apply(pool, &[points(false)], &[points(true)]).await;
    let v2 = HistTable::from_snapshot("s1", &points(true)).unwrap();
    assert!(
        !std::ptr::eq(v1.schema(), v2.schema()),
        "two leaked schemas"
    );
    assert_eq!(v1.schema().table, v2.schema().table, "one physical table");
    let again = HistTable::from_snapshot("s1", &points(false)).unwrap();
    assert!(std::ptr::eq(v1.schema(), again.schema()), "interned");

    let rows = v2.query().fetch(pool).await.unwrap();
    assert_eq!(rows[0].get("note"), Some(&SqlValue::Null));
    v2.query()
        .filter_eq("id", 1_i64)
        .update(pool, &[("note", "added".into())])
        .await
        .unwrap();
    // The old state still reads and writes the table, blind to `note`.
    v1.insert(pool, &[("id", 2_i64.into()), ("title", "two".into())])
        .await
        .unwrap();
    let old = v1.query().order_by("id", false).fetch(pool).await.unwrap();
    assert_eq!(old.len(), 2);
    assert!(old[0].get("note").is_none());
    let e = v1
        .query()
        .filter_eq("note", "added")
        .count(pool)
        .await
        .unwrap_err();
    assert!(matches!(e, HistError::UnknownColumn { .. }), "{e}");
    assert_eq!(
        v2.query()
            .filter_eq("note", "added")
            .count(pool)
            .await
            .unwrap(),
        1
    );
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

/// Criterion 3: on an audited generic-M2M owner, a historical delete and
/// update are one statement each, add no audit row and leave the junction
/// alone; the shared paths given the leaked schema find no model either.
async fn hist_delete_on_audited_generic_m2m_owner_is_one_statement(pool: &Pool) {
    rustango::contenttypes::ensure_seeded(pool).await.unwrap();
    rustango::contenttypes::clear_cache();
    for id in 1..=4 {
        let mut a = S1Gaudited {
            id,
            label: "a".into(),
        };
        a.insert(pool).await.unwrap();
        a.tags_m2m().set(&[id, 10], pool).await.unwrap();
    }
    let (links, audits) = (taggable_rows(pool).await, audit_rows(pool).await);
    assert_eq!(links, 8);

    let state = SchemaSnapshot::from_models_forced(&[S1Gaudited::SCHEMA]);
    let snap = state
        .tables
        .iter()
        .find(|t| t.name == "s1_gaudited")
        .unwrap();
    assert_eq!(
        snap.model, "S1Gaudited",
        "the snapshot carries the Rust name"
    );
    let t = HistTable::from_snapshot("s1", snap).unwrap();
    assert!(
        t.schema().name.starts_with(HIST_PREFIX),
        "{}",
        t.schema().name
    );
    assert!(
        ModelEntry::for_schema(t.schema()).is_none(),
        "the leaked schema matches a registered model"
    );

    let stmt = t
        .query()
        .filter_eq("id", 1_i64)
        .compile_delete(pool.dialect())
        .unwrap();
    assert!(stmt.sql.starts_with("DELETE FROM"), "{}", stmt.sql);
    let n = assert_num_queries(1, t.query().filter_eq("id", 1_i64).delete(pool)).await;
    assert_eq!(n.unwrap(), 1);
    let n = assert_num_queries(
        1,
        t.query()
            .filter_eq("id", 2_i64)
            .update(pool, &[("label", "b".into())]),
    )
    .await;
    assert_eq!(n.unwrap(), 1);
    assert_eq!(
        taggable_rows(pool).await,
        links,
        "hist delete touched the junction"
    );
    assert_eq!(audit_rows(pool).await, audits, "hist write was audited");

    // Shared, inventory-routed entry points handed the leaked schema.
    let id = |n: i64| WhereExpr::and_predicates(vec![Filter::new("id", Op::Eq, n)]);
    audit::delete(pool, &DeleteQuery::new(t.schema(), id(3)))
        .await
        .unwrap();
    let set = vec![Assignment::new("label", SqlValue::String("c".into()))];
    audit::update(pool, &UpdateQuery::new(t.schema(), set, id(4)))
        .await
        .unwrap();
    assert_eq!(
        taggable_rows(pool).await,
        links,
        "a shared delete matched the model and cleaned its junction"
    );
    assert_eq!(audit_rows(pool).await, audits, "a shared write was audited");
}

/// A second tenant with the same table: PG another schema (search_path
/// only that schema), MySQL another database, SQLite another memory DB.
async fn tenant_b(pool: &Pool) -> Pool {
    use rustango::testkit::matrix::Backend;
    let with_query = |url: String, q: &str| {
        let sep = if url.contains('?') { '&' } else { '?' };
        format!("{url}{sep}{q}")
    };
    match backend(pool) {
        "postgres" => {
            for sql in [
                "DROP SCHEMA IF EXISTS s1_tenant_b CASCADE",
                "CREATE SCHEMA s1_tenant_b",
            ] {
                rustango::sql::raw_execute_pool(pool, sql, Vec::new())
                    .await
                    .unwrap();
            }
            let url = Backend::Postgres.url().unwrap();
            Pool::connect(&with_query(url, "options=-c%20search_path%3Ds1_tenant_b"))
                .await
                .unwrap()
        }
        "mysql" => {
            let url = Backend::MySql.url().unwrap();
            let (base, db) = url.rsplit_once('/').unwrap();
            let db = db.split('?').next().unwrap();
            let name = format!("{}-tb", db.replace('_', "-"));
            let dbn = rustango::sql::server::DatabaseName::new(name.clone()).unwrap();
            let d = pool.dialect();
            let drop = d.drop_database_sql(&dbn).unwrap();
            let create = d.create_database_sql(&dbn, None).unwrap();
            for sql in [drop, create] {
                rustango::sql::raw_execute_pool(pool, &sql, Vec::new())
                    .await
                    .unwrap();
            }
            Pool::connect(&format!("{base}/{name}")).await.unwrap()
        }
        _ => Pool::connect("sqlite::memory:").await.unwrap(),
    }
}

fn tenant_snap() -> TableSnapshot {
    snap(json!({"name": "s1_tenant", "model": "Tn", "fields": [
        with(f("id", "i64"), json!({"primary_key": true})),
        with(f("label", "string"), json!({"max_length": 32})),
    ]}))
}

async fn tenant_work(pool: Pool, tag: &'static str, deletes: Vec<i64>, new_id: i64) {
    let t = HistTable::from_snapshot("s1", &tenant_snap()).unwrap();
    for round in 0..20_i64 {
        for id in 1..=20_i64 {
            t.query()
                .filter_eq("id", id)
                .update(&pool, &[("label", format!("{tag}{round}").into())])
                .await
                .unwrap();
            tokio::task::yield_now().await;
        }
    }
    t.query()
        .filter_in("id", deletes.into_iter().map(SqlValue::I64).collect())
        .delete(&pool)
        .await
        .unwrap();
    t.insert(&pool, &[("id", new_id.into()), ("label", tag.into())])
        .await
        .unwrap();
}

/// Criterion 4: K=2 tenants, same table and row ids, in parallel.
async fn hist_two_tenants_in_parallel_never_cross(pool: &Pool) {
    let b = tenant_b(pool).await;
    for p in [pool, &b] {
        apply(p, &[], &[tenant_snap()]).await;
        let t = HistTable::from_snapshot("s1", &tenant_snap()).unwrap();
        for id in 1..=20_i64 {
            t.insert(p, &[("id", id.into()), ("label", "seed".into())])
                .await
                .unwrap();
        }
    }
    let a_task = tokio::spawn(tenant_work(pool.clone(), "A", (1..=5).collect(), 100));
    let b_task = tokio::spawn(tenant_work(b.clone(), "B", (16..=20).collect(), 200));
    a_task.await.unwrap();
    b_task.await.unwrap();

    let t = HistTable::from_snapshot("s1", &tenant_snap()).unwrap();
    for (p, tag, gone, kept_new, foreign_new) in [
        (pool, "A", 1..=5_i64, 100_i64, 200_i64),
        (&b, "B", 16..=20, 200, 100),
    ] {
        let rows = t.query().order_by("id", false).fetch(p).await.unwrap();
        assert_eq!(rows.len(), 16, "{tag}: rows");
        for r in &rows {
            let SqlValue::String(label) = r.get("label").unwrap() else {
                panic!("label")
            };
            assert!(label.starts_with(tag), "{tag} sees {label}: a cross write");
        }
        for id in gone {
            assert_eq!(t.query().filter_eq("id", id).count(p).await.unwrap(), 0);
        }
        assert_eq!(
            t.query().filter_eq("id", kept_new).count(p).await.unwrap(),
            1
        );
        assert_eq!(
            t.query()
                .filter_eq("id", foreign_new)
                .count(p)
                .await
                .unwrap(),
            0
        );
    }
    b.close().await;
}

/// Criterion 5: a cell that does not read as its state type is an error.
async fn hist_strict_decode_fails_on_a_bad_cell(pool: &Pool) {
    let wide = snap(json!({"name": "s1_bad", "model": "Bad", "fields": [
        with(f("id", "i64"), json!({"primary_key": true})), f("n", "i64"),
    ]}));
    let narrow = snap(json!({"name": "s1_bad", "model": "Bad", "fields": [
        with(f("id", "i64"), json!({"primary_key": true})), f("n", "i16"),
    ]}));
    apply(pool, &[], &[wide.clone()]).await;
    let w = HistTable::from_snapshot("s1", &wide).unwrap();
    w.insert(pool, &[("id", 1_i64.into()), ("n", 100_000_i64.into())])
        .await
        .unwrap();
    // State says i16; the DB holds a value no i16 can.
    let n = HistTable::from_snapshot("s1", &narrow).unwrap();
    let got = n.query().fetch(pool).await;
    eprintln!("{}: i16 over 100000 -> {got:?}", backend(pool));
    assert!(got.is_err(), "{}: silent decode {got:?}", backend(pool));
    // The lenient JSON path, for contrast, turns it into NULL.
    let fields: Vec<&'static FieldSchema> = n.schema().fields.iter().collect();
    let lenient = rustango::sql::select_rows_as_json(pool, &SelectQuery::new(n.schema()), &fields)
        .await
        .unwrap();
    eprintln!("{}: lenient read -> {lenient:?}", backend(pool));

    if backend(pool) == "sqlite" {
        let k = HistTable::from_snapshot("s1", &kinds()).unwrap();
        apply(pool, &[], &[kinds()]).await;
        k.insert(pool, &kinds_row(1)).await.unwrap();
        for (sql, col) in [
            ("UPDATE s1_kinds SET c_json = 'not json'", "c_json"),
            ("UPDATE s1_kinds SET c_json = '{}', c_i32 = 'abc'", "c_i32"),
            ("UPDATE s1_kinds SET c_i32 = 1, c_dt = 'yesterday'", "c_dt"),
        ] {
            rustango::sql::raw_execute_pool(pool, sql, Vec::new())
                .await
                .unwrap();
            let e = k.query().fetch(pool).await.unwrap_err();
            assert!(
                matches!(&e, HistError::BadCell { column, .. } if *column == col),
                "{sql}: {e}"
            );
        }
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        hist_types_round_trip_and_filters,
        hist_json_eq_filter,
        hist_two_state_points_one_table,
        hist_delete_on_audited_generic_m2m_owner_is_one_statement,
        hist_two_tenants_in_parallel_never_cross,
        hist_strict_decode_fails_on_a_bad_cell,
    ]
}

/// Criterion 6: leaked bytes per distinct state, and none on re-intern.
#[test]
fn hist_leaked_bytes_report() {
    let _g = rustango::testkit::matrix::live_lock().blocking_lock();
    let mut k = kinds();
    k.name = "s1_leak_kinds".into();
    let before = leaked_bytes();
    let a = HistTable::from_snapshot("s1", &k).unwrap();
    let one = leaked_bytes() - before;
    let mut k2 = k.clone();
    k2.fields.push(
        snap(json!({"name": "x", "model": "X", "fields": [
            with(f("c_added", "string"), json!({"max_length": 32, "nullable": true}))
        ]}))
        .fields
        .remove(0),
    );
    HistTable::from_snapshot("s1", &k2).unwrap();
    let two = leaked_bytes() - before;
    // Field order and Rust names do not change the key.
    let mut shuffled = k.clone();
    shuffled.fields.reverse();
    shuffled.model = "Renamed".into();
    for _ in 0..1000 {
        let again = HistTable::from_snapshot("s1", &shuffled).unwrap();
        assert!(std::ptr::eq(a.schema(), again.schema()));
    }
    assert_eq!(leaked_bytes() - before, two, "re-interning leaked");
    eprintln!(
        "leaked: {one} B for a 16-column table, {two} B for two states; \
         FieldSchema = {} B, ModelSchema = {} B",
        std::mem::size_of::<FieldSchema>(),
        std::mem::size_of::<ModelSchema>()
    );
}
