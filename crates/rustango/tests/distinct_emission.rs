//! `.distinct()` / `.distinct_on(*fields)` — closes #264 / T1.2.
//!
//! Pins per-dialect SQL emission for:
//!   1. `.distinct()` → `SELECT DISTINCT ...` (uniform on PG / MySQL / SQLite).
//!   2. `.distinct_on(cols)` → `SELECT DISTINCT ON (cols) ...` on PG; portable
//!      `ROW_NUMBER() OVER (PARTITION BY cols ORDER BY <order>) AS __rn`
//!      subquery wrapper on MySQL / SQLite with outer `WHERE __rn = 1`.
//!   3. `.distinct_on` requires the keys at the head of `.order_by(...)`
//!      (Django parity — without the order, "first row per group" is
//!      non-deterministic).

use rustango::query::QuerySet;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "distinct_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    author_id: i64,
    created: chrono::DateTime<chrono::Utc>,
}

// ---------- .distinct() — uniform syntax ----------

#[test]
fn plain_distinct_pg() {
    let qs = QuerySet::<Post>::default().distinct();
    let sql = Postgres.compile_select(&qs.compile().unwrap()).unwrap().sql;
    assert!(sql.starts_with("SELECT DISTINCT "), "got: {sql}");
}

#[test]
fn plain_distinct_mysql() {
    let qs = QuerySet::<Post>::default().distinct();
    let sql = MySql.compile_select(&qs.compile().unwrap()).unwrap().sql;
    assert!(sql.starts_with("SELECT DISTINCT "), "got: {sql}");
}

#[test]
fn plain_distinct_sqlite() {
    let qs = QuerySet::<Post>::default().distinct();
    let sql = Sqlite.compile_select(&qs.compile().unwrap()).unwrap().sql;
    assert!(sql.starts_with("SELECT DISTINCT "), "got: {sql}");
}

// ---------- .distinct_on(cols) — PG native ----------

#[test]
fn distinct_on_pg_emits_native_syntax() {
    let qs = QuerySet::<Post>::default()
        .distinct_on(&["author_id"])
        .order_by(&[("author_id", false), ("created", true)]);
    let sql = Postgres.compile_select(&qs.compile().unwrap()).unwrap().sql;
    assert!(
        sql.contains(r#"DISTINCT ON ("author_id")"#),
        "PG should emit DISTINCT ON: {sql}"
    );
    assert!(
        sql.contains(r#"ORDER BY "author_id", "created" DESC"#),
        "outer ORDER BY preserved: {sql}"
    );
}

// ---------- .distinct_on(cols) — portable fallback ----------

#[test]
fn distinct_on_mysql_uses_row_number_subquery() {
    let qs = QuerySet::<Post>::default()
        .distinct_on(&["author_id"])
        .order_by(&[("author_id", false), ("created", true)]);
    let sql = MySql.compile_select(&qs.compile().unwrap()).unwrap().sql;
    // Outer SELECT only over projection columns.
    assert!(
        sql.starts_with("SELECT `id`, `title`, `author_id`, `created` FROM ("),
        "MySQL fallback should wrap in subquery: {sql}"
    );
    // Inner: ROW_NUMBER() OVER (PARTITION BY ... ORDER BY ...).
    assert!(
        sql.contains("ROW_NUMBER() OVER (PARTITION BY `author_id` ORDER BY `author_id`, `created` DESC) AS __rn"),
        "MySQL fallback should emit ROW_NUMBER window: {sql}"
    );
    assert!(
        sql.contains(") sub WHERE sub.__rn = 1"),
        "MySQL fallback should filter __rn=1: {sql}"
    );
}

#[test]
fn distinct_on_sqlite_uses_row_number_subquery() {
    let qs = QuerySet::<Post>::default()
        .distinct_on(&["author_id"])
        .order_by(&[("author_id", false), ("created", true)]);
    let sql = Sqlite.compile_select(&qs.compile().unwrap()).unwrap().sql;
    assert!(
        sql.contains(
            r#"ROW_NUMBER() OVER (PARTITION BY "author_id" ORDER BY "author_id", "created" DESC) AS __rn"#
        ),
        "SQLite fallback should emit ROW_NUMBER window: {sql}"
    );
    assert!(
        sql.contains(r#") sub WHERE sub.__rn = 1"#),
        "SQLite fallback should filter __rn=1: {sql}"
    );
}

// ---------- Order-by validation ----------

#[test]
fn distinct_on_requires_keys_at_head_of_order_by() {
    let qs = QuerySet::<Post>::default()
        .distinct_on(&["author_id"])
        .order_by(&[("created", true)]);
    let err = qs.compile().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("DistinctOn") || msg.contains("distinct_on") || msg.contains("order_by"),
        "expected DistinctOnOrderByMismatch error, got: {err:?}"
    );
}

#[test]
fn distinct_on_empty_columns_is_rejected() {
    let qs = QuerySet::<Post>::default()
        .distinct_on(&[])
        .order_by(&[("id", false)]);
    let err = qs.compile().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("at least one column"),
        "expected DistinctOnEmpty error, got: {err:?}"
    );
}

#[test]
fn distinct_on_unknown_column_is_rejected() {
    let qs = QuerySet::<Post>::default()
        .distinct_on(&["no_such_field"])
        .order_by(&[("no_such_field", false)]);
    let err = qs.compile().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("no_such_field") || msg.contains("UnknownField"),
        "expected UnknownField error, got: {err:?}"
    );
}

// ---------- Tri-dialect: same query, three correct shapes ----------

#[test]
fn same_query_produces_correct_per_dialect_sql() {
    let make_qs = || {
        QuerySet::<Post>::default()
            .distinct_on(&["author_id"])
            .order_by(&[("author_id", false), ("created", true)])
    };
    let pg = Postgres
        .compile_select(&make_qs().compile().unwrap())
        .unwrap()
        .sql;
    let my = MySql
        .compile_select(&make_qs().compile().unwrap())
        .unwrap()
        .sql;
    let lite = Sqlite
        .compile_select(&make_qs().compile().unwrap())
        .unwrap()
        .sql;
    assert!(pg.contains("DISTINCT ON"), "PG native: {pg}");
    assert!(my.contains("ROW_NUMBER()"), "MySQL fallback: {my}");
    assert!(lite.contains("ROW_NUMBER()"), "SQLite fallback: {lite}");
}
