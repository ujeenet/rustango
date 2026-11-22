//! SPIKE S6 (migrations engine v2): catalog reader, normalisers, probes
//! and the 2.6 / 14.1 default rules on every backend. Throwaway.

#![cfg(all(
    feature = "migrate-v2-spike",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use rustango::migrate::v2_spike::catalog::{read_table, LiveTable};
use rustango::migrate::v2_spike::defaults::{check, run_default, Backends};
use rustango::migrate::v2_spike::norm::{self, DefaultCtx};
use rustango::migrate::v2_spike::ops::{
    render, CheckDecl, ColumnDecl, FkDecl, IndexDecl, SpikeOp, TableDecl,
};
use rustango::migrate::v2_spike::probe::{not_expressible, probe, spec, ProbeSpec, Prop, Verdict};
use rustango::sql::{
    raw_execute_pool, raw_execute_tx, raw_query_pool, Dialect as _, DialectKind, Pool,
};
use rustango::testkit::matrix::drop_table;
use rustango::tri_dialect_test;

const TABLES: &[&str] = &["s6_child", "s6_alt", "s6_parent", "s6_defaults", "s6_ix"];

async fn clean(pool: &Pool) {
    for t in TABLES {
        drop_table(pool, t).await;
    }
}

fn kind(pool: &Pool) -> DialectKind {
    pool.dialect().kind()
}

async fn exec(pool: &Pool, stmts: Vec<String>) {
    for s in stmts {
        raw_execute_pool(pool, &s, Vec::new())
            .await
            .unwrap_or_else(|e| panic!("{}: {s}: {e}", pool.dialect().name()));
    }
}

async fn apply(pool: &Pool, op: &SpikeOp) {
    exec(pool, render(pool.dialect(), op)).await;
}

/// SQLite changes checks, FKs and column types by rebuild (S3); here a
/// drop and re-create from the after-state stands in for it.
async fn rebuild(pool: &Pool, t: &TableDecl) {
    drop_table(pool, &t.name).await;
    apply(pool, &SpikeOp::CreateTable(t.clone())).await;
}

fn c(name: &str, ty: &str) -> ColumnDecl {
    ColumnDecl::new(name, ty)
}

trait Col {
    fn max(self, n: u32) -> Self;
    fn not_null(self) -> Self;
    fn default(self, d: &str) -> Self;
    fn gen(self, e: &str) -> Self;
    fn comment(self, s: &str) -> Self;
    fn pk(self) -> Self;
}

impl Col for ColumnDecl {
    fn max(mut self, n: u32) -> Self {
        self.max_length = Some(n);
        self
    }
    fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }
    fn default(mut self, d: &str) -> Self {
        self.default = Some(d.into());
        self
    }
    fn gen(mut self, e: &str) -> Self {
        self.generated = Some(e.into());
        self
    }
    fn comment(mut self, s: &str) -> Self {
        self.comment = Some(s.into());
        self
    }
    fn pk(mut self) -> Self {
        self.primary_key = true;
        self.nullable = false;
        self
    }
}

fn table(name: &str, columns: Vec<ColumnDecl>) -> TableDecl {
    TableDecl {
        name: name.into(),
        columns,
        comment: None,
        checks: vec![],
        fks: vec![],
    }
}

fn index(name: &str, table: &str, cols: &[&str], unique: bool) -> IndexDecl {
    IndexDecl {
        name: name.into(),
        table: table.into(),
        columns: cols.iter().map(|s| (*s).to_owned()).collect(),
        unique,
        method: "btree".into(),
        predicate: None,
        include: vec![],
    }
}

async fn verdict(pool: &Pool, op: &SpikeOp) -> Verdict {
    probe(pool, op)
        .await
        .unwrap_or_else(|e| panic!("{}: probe {op:?}: {e}", pool.dialect().name()))
        .verdict
}

async fn expect(pool: &Pool, op: &SpikeOp, want: &str) {
    let v = verdict(pool, op).await;
    let got = match &v {
        Verdict::Present => "present",
        Verdict::Absent => "absent",
        Verdict::Partial(_) => "partial",
    };
    assert_eq!(got, want, "{}: {op:?} -> {v:?}", pool.dialect().name());
}

fn partial_props(v: &Verdict) -> Vec<Prop> {
    match v {
        Verdict::Partial(m) => m.iter().map(|m| m.prop).collect(),
        _ => vec![],
    }
}

async fn live(pool: &Pool, t: &str) -> LiveTable {
    read_table(pool, t)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{t} missing"))
}

fn ne(d: DialectKind, s: &str) -> String {
    norm::norm_expr(d, s).unwrap()
}

// ---------------------------------------------------------------------------
// Criterion 1: the reader sees every object kind the catalog can express.

async fn reader_reads_every_object(pool: &Pool) {
    let k = kind(pool);
    let mut parent = table(
        "s6_parent",
        vec![
            c("id", "i64").pk(),
            c("code", "string")
                .max(20)
                .not_null()
                .default("'en'")
                .comment("code c"),
            c("qty", "i64").not_null().default("0"),
            c("dbl", "i64").gen("qty * 2"),
        ],
    );
    parent.comment = Some("parent c".into());
    parent.checks = vec![CheckDecl {
        name: "s6_parent_qty_pos".into(),
        expr: "qty >= 0".into(),
    }];
    apply(pool, &SpikeOp::CreateTable(parent)).await;
    let mut child = table("s6_child", vec![c("id", "i64").pk(), c("parent_id", "i64")]);
    child.fks = vec![FkDecl {
        name: "s6_child_parent_fk".into(),
        columns: vec!["parent_id".into()],
        ref_table: "s6_parent".into(),
        ref_columns: vec!["id".into()],
        on_delete: Some("SET NULL".into()),
    }];
    apply(pool, &SpikeOp::CreateTable(child)).await;
    apply(
        pool,
        &SpikeOp::AddIndex(index("s6_parent_code_uq", "s6_parent", &["code"], true)),
    )
    .await;
    let mut hash = index("s6_parent_qty_h", "s6_parent", &["qty"], false);
    hash.method = "hash".into();
    apply(pool, &SpikeOp::AddIndex(hash)).await;
    if k != DialectKind::Mysql {
        let mut part = index("s6_parent_qty_p", "s6_parent", &["qty"], false);
        part.predicate = Some("qty > 5".into());
        apply(pool, &SpikeOp::AddIndex(part)).await;
    }

    let p = live(pool, "s6_parent").await;
    let names: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "code", "qty", "dbl"]);
    let code = &p.columns[1];
    assert!(!code.nullable);
    assert_eq!(
        norm::norm_default(k, code.default.as_deref().unwrap(), DefaultCtx::default()).unwrap(),
        "'en'"
    );
    let commented = k != DialectKind::Sqlite;
    assert_eq!(code.comment.as_deref(), commented.then_some("code c"));
    assert_eq!(p.comment.as_deref(), commented.then_some("parent c"));
    let dbl = p.columns[3].generated.as_ref().expect("generated column");
    assert!(dbl.stored);
    assert_eq!(ne(k, &dbl.expr), ne(k, "qty * 2"), "raw {:?}", dbl.expr);
    assert_eq!(p.checks.len(), 1, "{:?}", p.checks);
    assert_eq!(p.checks[0].name.as_deref(), Some("s6_parent_qty_pos"));
    assert_eq!(
        ne(k, &p.checks[0].expr),
        ne(k, "qty >= 0"),
        "raw {:?}",
        p.checks[0].expr
    );
    assert!(p.checks[0].validated);
    let ix = |n: &str| {
        p.indexes
            .iter()
            .find(|i| i.name == n)
            .unwrap_or_else(|| panic!("{n} in {:?}", p.indexes))
    };
    assert!(ix("s6_parent_code_uq").unique);
    assert_eq!(ix("s6_parent_code_uq").columns, ["code"]);
    // InnoDB builds a btree for USING HASH, so MySQL cannot probe the method.
    let want_method = if k == DialectKind::Postgres {
        "hash"
    } else {
        "btree"
    };
    assert_eq!(ix("s6_parent_qty_h").method, want_method);
    if k != DialectKind::Mysql {
        let pred = ix("s6_parent_qty_p").predicate.clone().expect("predicate");
        assert_eq!(ne(k, &pred), ne(k, "qty > 5"));
    }
    assert!(p.indexes.iter().all(|i| i.valid));

    let ch = live(pool, "s6_child").await;
    assert_eq!(ch.fks.len(), 1, "{:?}", ch.fks);
    let fk = &ch.fks[0];
    assert_eq!(fk.name.as_deref(), Some("s6_child_parent_fk"));
    assert_eq!(fk.columns, ["parent_id"]);
    assert_eq!(
        (fk.ref_table.as_str(), fk.ref_columns.as_slice()),
        ("s6_parent", &["id".to_owned()][..])
    );
    assert_eq!(fk.on_delete, "SET NULL");
    assert!(read_table(pool, "s6_nope").await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Criterion 3: probe answers for every op, before and after.

async fn probes_answer_for_every_op(pool: &Pool) {
    let k = kind(pool);
    let lite = k == DialectKind::Sqlite;
    apply(
        pool,
        &SpikeOp::CreateTable(table("s6_ix", vec![c("id", "i64").pk()])),
    )
    .await;
    let mut cur = table(
        "s6_alt",
        vec![
            c("id", "i64").pk(),
            c("a", "i64").not_null().default("0"),
            c("b", "string").max(20),
        ],
    );
    let create = SpikeOp::CreateTable(cur.clone());
    assert!(matches!(spec(&create, k), ProbeSpec::Props(ref p) if p.contains(&Prop::ColDefault)));
    expect(pool, &create, "absent").await;
    apply(pool, &create).await;
    expect(pool, &create, "present").await;

    // add_column, and a half-applied one (default missing) is Partial.
    let col_c = c("c", "string").max(10).not_null().default("'x'");
    let add = SpikeOp::AddColumn {
        table: "s6_alt".into(),
        column: col_c.clone(),
    };
    expect(pool, &add, "absent").await;
    apply(pool, &add).await;
    cur.columns.push(col_c.clone());
    expect(pool, &add, "present").await;
    if !lite {
        exec(
            pool,
            vec!["ALTER TABLE s6_alt ALTER COLUMN c DROP DEFAULT".into()],
        )
        .await;
        let v = verdict(pool, &add).await;
        assert_eq!(partial_props(&v), [Prop::ColDefault], "{v:?}");
        exec(
            pool,
            vec!["ALTER TABLE s6_alt ALTER COLUMN c SET DEFAULT 'x'".into()],
        )
        .await;
        expect(pool, &add, "present").await;
    }

    // alter_column: Absent, Partial after the first PG statement, Present.
    let from = c("b", "string").max(20);
    let to = c("b", "string").max(40).not_null().default("'z'");
    let alter = SpikeOp::AlterColumn {
        table: "s6_alt".into(),
        from,
        to: to.clone(),
    };
    expect(pool, &alter, "absent").await;
    match k {
        DialectKind::Sqlite => {
            cur.columns[2] = to;
            rebuild(pool, &cur).await;
        }
        DialectKind::Postgres => {
            let stmts = render(pool.dialect(), &alter);
            exec(pool, stmts[..1].to_vec()).await;
            let v = verdict(pool, &alter).await;
            assert_eq!(
                partial_props(&v),
                [Prop::ColNullable, Prop::ColDefault],
                "{v:?}"
            );
            exec(pool, stmts[1..].to_vec()).await;
            cur.columns[2] = to;
        }
        DialectKind::Mysql => {
            apply(pool, &alter).await;
            cur.columns[2] = to;
        }
    }
    expect(pool, &alter, "present").await;

    // add_index / drop_index (a unique is a unique index).
    let ix = index("s6_alt_a_uq", "s6_alt", &["a"], true);
    let add_ix = SpikeOp::AddIndex(ix.clone());
    expect(pool, &add_ix, "absent").await;
    apply(pool, &add_ix).await;
    expect(pool, &add_ix, "present").await;
    let mut other = ix.clone();
    other.unique = false;
    let v = verdict(pool, &SpikeOp::AddIndex(other)).await;
    assert_eq!(partial_props(&v), [Prop::IndexUnique]);
    let drop_ix = SpikeOp::DropIndex { was: ix };
    expect(pool, &drop_ix, "absent").await;
    apply(pool, &drop_ix).await;
    expect(pool, &drop_ix, "present").await;

    // add_check / drop_check.
    let chk = CheckDecl {
        name: "s6_alt_a_pos".into(),
        expr: "a >= 0".into(),
    };
    let add_chk = SpikeOp::AddCheck {
        table: "s6_alt".into(),
        check: chk.clone(),
    };
    expect(pool, &add_chk, "absent").await;
    cur.checks.push(chk.clone());
    if lite {
        rebuild(pool, &cur).await
    } else {
        apply(pool, &add_chk).await
    }
    expect(pool, &add_chk, "present").await;
    let wrong = SpikeOp::AddCheck {
        table: "s6_alt".into(),
        check: CheckDecl {
            expr: "a >= 1".into(),
            ..chk.clone()
        },
    };
    assert_eq!(
        partial_props(&verdict(pool, &wrong).await),
        [Prop::CheckExpr]
    );
    let drop_chk = SpikeOp::DropCheck {
        table: "s6_alt".into(),
        was: chk,
    };
    expect(pool, &drop_chk, "absent").await;
    cur.checks.clear();
    if lite {
        rebuild(pool, &cur).await
    } else {
        apply(pool, &drop_chk).await
    }
    expect(pool, &drop_chk, "present").await;

    // add_fk (PG: NOT VALID is Partial until VALIDATE) / drop_fk.
    let fk = FkDecl {
        name: "s6_alt_a_fk".into(),
        columns: vec!["a".into()],
        ref_table: "s6_ix".into(),
        ref_columns: vec!["id".into()],
        on_delete: Some("CASCADE".into()),
    };
    let add_fk = SpikeOp::AddFk {
        table: "s6_alt".into(),
        fk: fk.clone(),
    };
    expect(pool, &add_fk, "absent").await;
    match k {
        DialectKind::Sqlite => {
            cur.fks.push(fk.clone());
            rebuild(pool, &cur).await;
        }
        DialectKind::Postgres => {
            let s = render(pool.dialect(), &add_fk).remove(0);
            exec(pool, vec![format!("{s} NOT VALID")]).await;
            assert_eq!(
                partial_props(&verdict(pool, &add_fk).await),
                [Prop::FkValidated]
            );
            exec(
                pool,
                vec!["ALTER TABLE s6_alt VALIDATE CONSTRAINT s6_alt_a_fk".into()],
            )
            .await;
        }
        DialectKind::Mysql => apply(pool, &add_fk).await,
    }
    expect(pool, &add_fk, "present").await;
    let set_null = SpikeOp::AddFk {
        table: "s6_alt".into(),
        fk: FkDecl {
            on_delete: Some("SET NULL".into()),
            ..fk.clone()
        },
    };
    assert_eq!(
        partial_props(&verdict(pool, &set_null).await),
        [Prop::FkOnDelete]
    );
    let drop_fk = SpikeOp::DropFk {
        table: "s6_alt".into(),
        was: fk,
    };
    expect(pool, &drop_fk, "absent").await;
    cur.fks.clear();
    if lite {
        rebuild(pool, &cur).await
    } else {
        apply(pool, &drop_fk).await
    }
    expect(pool, &drop_fk, "present").await;

    // drop_column / drop_table.
    let drop_col = SpikeOp::DropColumn {
        table: "s6_alt".into(),
        was: col_c,
    };
    expect(pool, &drop_col, "absent").await;
    apply(pool, &drop_col).await;
    cur.columns.retain(|c| c.name != "c");
    expect(pool, &drop_col, "present").await;
    let drop_t = SpikeOp::DropTable { was: cur };
    expect(pool, &drop_t, "absent").await;
    apply(pool, &drop_t).await;
    expect(pool, &drop_t, "present").await;
}

// ---------------------------------------------------------------------------
// PG CREATE INDEX CONCURRENTLY that fails leaves an invalid index: Partial.

async fn invalid_concurrent_index_is_partial(pool: &Pool) {
    let k = kind(pool);
    apply(
        pool,
        &SpikeOp::CreateTable(table("s6_ix", vec![c("id", "i64").pk(), c("v", "i64")])),
    )
    .await;
    let op = SpikeOp::AddIndex(index("s6_ix_v_uq", "s6_ix", &["v"], true));
    if k != DialectKind::Postgres {
        assert!(not_expressible(k)
            .iter()
            .any(|(p, _)| *p == Prop::IndexValid));
        expect(pool, &op, "absent").await;
        apply(pool, &op).await;
        expect(pool, &op, "present").await;
        return;
    }
    exec(
        pool,
        vec!["INSERT INTO s6_ix (id, v) VALUES (1, 7), (2, 7)".into()],
    )
    .await;
    let failed = raw_execute_pool(
        pool,
        "CREATE UNIQUE INDEX CONCURRENTLY s6_ix_v_uq ON s6_ix (v)",
        Vec::new(),
    )
    .await;
    assert!(failed.is_err(), "duplicates must fail the build");
    let v = verdict(pool, &op).await;
    assert_eq!(partial_props(&v), [Prop::IndexValid], "{v:?}");
}

// ---------------------------------------------------------------------------
// Criterion 4: every 2.6 row creates, inserts and reads back.

struct Row {
    col: ColumnDecl,
    /// Read-back expression; `{c}` is the quoted column.
    read: &'static str,
    want: Want,
}

enum Want {
    Text(&'static str),
    Zero,
    /// Epoch seconds within 300 s of now, plus an offset in hours.
    NowUtc(i64),
    Uuid36,
    Blob16V4,
}

fn rows(k: DialectKind) -> Vec<Row> {
    let txt = |name: &str, ty: &str, max: Option<u32>, def: &str, want: &'static str| {
        let mut col = c(name, ty).default(def);
        col.max_length = max;
        Row {
            col,
            read: "CAST({c} AS {T})",
            want: Want::Text(want),
        }
    };
    match k {
        DialectKind::Postgres => vec![
            txt("s", "string", Some(20), "'en'", "en"),
            txt("q", "string", Some(20), "'it''s'", "it's"),
            txt("i", "i64", None, "0", "0"),
            txt("d", "decimal", None, "0.00", "0.00"),
            txt("b", "bool", None, "TRUE", "true"),
            txt("dt", "date", None, "'2026-01-01'", "2026-01-01"),
            Row {
                col: c("ts", "datetime").default("now()"),
                read: "CAST(EXTRACT(EPOCH FROM {c}) AS BIGINT)",
                want: Want::NowUtc(0),
            },
            Row {
                col: c("u", "uuid").default("gen_random_uuid()"),
                read: "CAST({c} AS TEXT)",
                want: Want::Uuid36,
            },
            txt("j", "json", None, "'{}'::jsonb", "{}"),
            txt("ja", "json", None, "'[]'::jsonb", "[]"),
        ],
        DialectKind::Mysql => vec![
            txt("s", "string", Some(20), "'en'", "en"),
            txt("lt", "string", None, "('en')", "en"),
            txt("q", "string", Some(20), "'it''s'", "it's"),
            txt("i", "i64", None, "0", "0"),
            Row {
                col: c("d", "decimal").default("0.00"),
                read: "CAST({c} AS CHAR)",
                want: Want::Zero,
            },
            txt("b", "bool", None, "TRUE", "1"),
            txt("dt", "date", None, "'2026-01-01'", "2026-01-01"),
            Row {
                col: c("ts", "datetime").default("(UTC_TIMESTAMP(6))"),
                read: "TIMESTAMPDIFF(SECOND, '1970-01-01', {c})",
                want: Want::NowUtc(0),
            },
            // Not a table row: shows why the table says UTC_TIMESTAMP (session is +05:00).
            Row {
                col: c("ts_cur", "datetime").default("CURRENT_TIMESTAMP(6)"),
                read: "TIMESTAMPDIFF(SECOND, '1970-01-01', {c})",
                want: Want::NowUtc(5),
            },
            Row {
                col: c("u", "uuid").default("(UUID())"),
                read: "CAST({c} AS CHAR)",
                want: Want::Uuid36,
            },
            txt("j", "json", None, "('{}')", "{}"),
            txt("ja", "json", None, "('[]')", "[]"),
        ],
        DialectKind::Sqlite => vec![
            txt("s", "string", Some(20), "'en'", "en"),
            txt("q", "string", Some(20), "'it''s'", "it's"),
            txt("i", "i64", None, "0", "0"),
            Row {
                col: c("d", "decimal").default("0.00"),
                read: "CAST({c} AS TEXT)",
                want: Want::Zero,
            },
            txt("b", "bool", None, "TRUE", "1"),
            txt("dt", "date", None, "'2026-01-01'", "2026-01-01"),
            Row {
                col: c("ts", "datetime").default(
                    &rustango::sql::Sqlite.translate_default_expr("now()", "datetime", None),
                ),
                read: "CAST(strftime('%s', {c}) AS INTEGER)",
                want: Want::NowUtc(0),
            },
            Row {
                col: c("u", "uuid").default(&rustango::sql::Sqlite.translate_default_expr(
                    "gen_random_uuid()",
                    "uuid",
                    None,
                )),
                read: "typeof({c}) || ':' || hex({c})",
                want: Want::Blob16V4,
            },
            txt("j", "json", None, "'{}'", "{}"),
            txt("ja", "json", None, "'[]'", "[]"),
        ],
    }
}

async fn defaults_table_rows_round_trip(pool: &Pool) {
    let k = kind(pool);
    let d = pool.dialect();
    let rows = rows(k);
    let mut cols = vec![c("id", "i64").pk()];
    cols.extend(rows.iter().map(|r| r.col.clone()));
    let t = table("s6_defaults", cols);
    let create = SpikeOp::CreateTable(t.clone());
    apply(pool, &create).await;
    // Criterion 2 live: every declared default equals its catalog text.
    expect(pool, &create, "present").await;
    {
        let mut conn = pool.acquire().await.unwrap();
        conn.close_on_drop();
        let mut tx = conn.begin().await.unwrap();
        if k == DialectKind::Mysql {
            raw_execute_tx(&mut tx, "SET time_zone = '+05:00'", Vec::new())
                .await
                .unwrap();
        }
        raw_execute_tx(
            &mut tx,
            "INSERT INTO s6_defaults (id) VALUES (1)",
            Vec::new(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let text_ty = if k == DialectKind::Mysql {
        "CHAR"
    } else {
        "TEXT"
    };
    let now = chrono::Utc::now().timestamp();
    for r in &rows {
        let expr = r
            .read
            .replace("{c}", &d.quote_ident(&r.col.name))
            .replace("{T}", text_ty);
        let sql = format!("SELECT CAST({expr} AS {text_ty}) FROM s6_defaults");
        let got: Vec<(Option<String>,)> = raw_query_pool(&sql, Vec::new(), pool).await.unwrap();
        let got = got[0]
            .0
            .clone()
            .unwrap_or_else(|| panic!("{k:?} {} read NULL", r.col.name));
        let what = format!("{k:?} {} = {:?}: got {got}", r.col.name, r.col.default);
        match r.want {
            Want::Text(w) => assert_eq!(got, w, "{what}"),
            Want::Zero => assert_eq!(got.parse::<f64>().unwrap(), 0.0, "{what}"),
            Want::NowUtc(h) => {
                let at: i64 = got.parse().unwrap();
                assert!((at - now - h * 3600).abs() < 300, "{what} (now {now})");
            }
            Want::Uuid36 => assert_eq!(got.len(), 36, "{what}"),
            Want::Blob16V4 => {
                let (ty, hex) = got.split_once(':').unwrap();
                assert_eq!((ty, hex.len(), &hex[12..13]), ("blob", 32, "4"), "{what}");
            }
        }
        // And `check` passes it on its own backend, live.
        let rep = check(&Backends::new(&[k]).unwrap(), &r.col, Some(pool)).await;
        assert!(rep.refusals.is_empty(), "{what}: {:?}", rep.refusals);
    }
    // SQLite keeps the column's text: the datetime default is in the canonical shape.
    if k == DialectKind::Sqlite {
        let ok: Vec<(i64,)> = raw_query_pool(
            "SELECT ts GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]+00:00' FROM s6_defaults",
            Vec::new(),
            pool,
        )
        .await
        .unwrap();
        assert_eq!(ok[0].0, 1);
    }
}

/// One string valid on all three: what a multi-backend project can write.
async fn portable_defaults_run_everywhere(pool: &Pool) {
    for (ty, max, text) in [
        ("json", None, "('{}')"),
        ("json", None, "('[]')"),
        ("string", None, "('en')"),
        ("string", Some(20), "'en'"),
        ("string", Some(20), "'it''s'"),
        ("bool", None, "TRUE"),
        ("i64", None, "-1"),
        ("decimal", None, "0.00"),
        ("date", None, "'2026-01-01'"),
    ] {
        let mut col = c("c", ty).default(text);
        col.max_length = max;
        run_default(pool, &col)
            .await
            .unwrap_or_else(|e| panic!("{:?} {text}: {e}", kind(pool)));
        let all = Backends::new(&[
            DialectKind::Postgres,
            DialectKind::Mysql,
            DialectKind::Sqlite,
        ])
        .unwrap();
        assert!(
            check(&all, &col, Some(pool)).await.refusals.is_empty(),
            "{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// The 14.1 rule: refused, naming the backend; offline rules match reality.

async fn default_check_refuses_naming_the_backend(pool: &Pool) {
    use DialectKind::{Mysql, Postgres, Sqlite};
    let k = kind(pool);
    let refused = |r: &rustango::migrate::v2_spike::defaults::Report| -> Vec<DialectKind> {
        let mut v: Vec<DialectKind> = r.refusals.iter().map(|x| x.backend).collect();
        v.dedup();
        v
    };
    // A PG-only cast in a PG+SQLite project.
    let jsonb = c("j", "json").default("'{}'::jsonb");
    let rep = check(
        &Backends::new(&[Postgres, Sqlite]).unwrap(),
        &jsonb,
        Some(pool),
    )
    .await;
    assert_eq!(refused(&rep), [Sqlite]);
    assert!(
        rep.refusals[0].to_string().contains("sqlite"),
        "{}",
        rep.refusals[0]
    );
    // Offline cannot know `now()` is missing on SQLite; the in-memory run does.
    let now = c("ts", "datetime").default("(now())");
    let rep = check(&Backends::new(&[Postgres, Sqlite]).unwrap(), &now, None).await;
    assert_eq!(refused(&rep), [Sqlite], "{rep:?}");
    // The connected backend's run catches a function the classifier does not know.
    let unknown = match k {
        Postgres => "(utc_date())",
        Mysql => "(no_such_fn())",
        Sqlite => "(no_such_fn())",
    };
    let col = c("x", "string").max(20).default(unknown);
    let rep = check(&Backends::new(&[k]).unwrap(), &col, Some(pool)).await;
    assert_eq!(refused(&rep), [k], "{rep:?}");
    // Each offline syntax refusal on this backend also fails for real.
    let offline_refused: &[(&str, Option<u32>, &str)] = match k {
        Postgres => &[
            ("datetime", None, "(UTC_TIMESTAMP(6))"),
            ("datetime", None, "(strftime('%s','now'))"),
        ],
        Mysql => &[
            ("string", None, "'en'"),
            ("json", None, "'{}'"),
            ("json", None, "'{}'::json"),
            ("uuid", None, "gen_random_uuid()"),
            ("datetime", None, "UTC_TIMESTAMP(6)"),
        ],
        Sqlite => &[
            ("datetime", None, "now()"),
            ("json", None, "'{}'::jsonb"),
            ("datetime", None, "strftime('%s','now')"),
        ],
    };
    for (ty, max, text) in offline_refused {
        let mut col = c("x", ty).default(text);
        col.max_length = *max;
        let rep = check(&Backends::new(&[k]).unwrap(), &col, None).await;
        assert!(rep.refused_on(k), "{k:?} {text} not refused offline");
        assert!(
            run_default(pool, &col).await.is_err(),
            "{k:?} {text} offline refuses but the server accepts"
        );
    }
    // Policy refusals: valid SQL, refused for a stated reason.
    if k == Sqlite {
        let col = c("ts", "datetime").default("CURRENT_TIMESTAMP");
        assert!(run_default(pool, &col).await.is_ok());
        assert!(check(&Backends::new(&[k]).unwrap(), &col, None)
            .await
            .refused_on(Sqlite));
    }
}

// ---------------------------------------------------------------------------
// Expressions the server rewrites compare equal only through a round-trip.

async fn roundtrip_compares_rewritten_expressions(pool: &Pool) {
    let k = kind(pool);
    let mut t = table(
        "s6_ix",
        vec![
            c("id", "i64").pk(),
            c("a", "i64"),
            c("b", "i64"),
            c("g", "i64").gen("a + b * 2"),
        ],
    );
    t.checks = vec![CheckDecl {
        name: "s6_ix_rt".into(),
        expr: "a IN (1, 2) AND b > 0".into(),
    }];
    let create = SpikeOp::CreateTable(t.clone());
    apply(pool, &create).await;
    let l = live(pool, "s6_ix").await;
    let raw_check = l.checks[0].expr.clone();
    let raw_gen = l.columns[3].generated.clone().unwrap().expr;
    if k != DialectKind::Sqlite {
        // The pure normaliser alone says "different" here.
        assert_ne!(
            ne(k, &raw_check),
            ne(k, "a IN (1, 2) AND b > 0"),
            "{raw_check}"
        );
        assert_ne!(ne(k, &raw_gen), ne(k, "a + b * 2"), "{raw_gen}");
    }
    expect(pool, &create, "present").await;
    let chk = |e: &str| SpikeOp::AddCheck {
        table: "s6_ix".into(),
        check: CheckDecl {
            name: "s6_ix_rt".into(),
            expr: e.into(),
        },
    };
    expect(pool, &chk("a IN (1, 2) AND b > 0"), "present").await;
    assert_eq!(
        partial_props(&verdict(pool, &chk("a IN (1, 3) AND b > 0")).await),
        [Prop::CheckExpr]
    );
    if k != DialectKind::Mysql {
        let mut ix = index("s6_ix_p", "s6_ix", &["a"], false);
        ix.predicate = Some("a IN (1, 2)".into());
        let op = SpikeOp::AddIndex(ix.clone());
        apply(pool, &op).await;
        expect(pool, &op, "present").await;
        ix.predicate = Some("a IN (1, 3)".into());
        assert_eq!(
            partial_props(&verdict(pool, &SpikeOp::AddIndex(ix)).await),
            [Prop::IndexPredicate]
        );
    }
}

tri_dialect_test!(
    setup: clean,
    scenarios: [
        reader_reads_every_object,
        probes_answer_for_every_op,
        invalid_concurrent_index_is_partial,
        defaults_table_rows_round_trip,
        portable_defaults_run_everywhere,
        default_check_refuses_naming_the_backend,
        roundtrip_compares_rewritten_expressions,
    ]
);

/// The probe-spec table for the report: compared properties per op and dialect.
#[test]
fn probe_spec_table() {
    let t = || table("t", vec![]);
    let col = c("c", "i64");
    let ix = index("i", "t", &["c"], false);
    let chk = CheckDecl {
        name: "k".into(),
        expr: "c > 0".into(),
    };
    let fk = FkDecl {
        name: "f".into(),
        columns: vec![],
        ref_table: "r".into(),
        ref_columns: vec![],
        on_delete: None,
    };
    let ops = [
        ("create_table/drop_table", SpikeOp::CreateTable(t())),
        (
            "add/drop/alter_column",
            SpikeOp::AddColumn {
                table: "t".into(),
                column: col,
            },
        ),
        ("add/drop_index (+unique)", SpikeOp::AddIndex(ix)),
        (
            "add/drop_check",
            SpikeOp::AddCheck {
                table: "t".into(),
                check: chk,
            },
        ),
        (
            "add/drop_fk",
            SpikeOp::AddFk {
                table: "t".into(),
                fk,
            },
        ),
    ];
    for (name, op) in &ops {
        for k in [
            DialectKind::Postgres,
            DialectKind::Mysql,
            DialectKind::Sqlite,
        ] {
            let ProbeSpec::Props(p) = spec(op, k) else {
                panic!()
            };
            assert!(!p.is_empty());
            println!("{name:26} {:9} {p:?}", k.name());
        }
    }
    for k in [DialectKind::Mysql, DialectKind::Sqlite] {
        println!("not expressible on {}: {:?}", k.name(), not_expressible(k));
    }
}
