//! Tri-dialect emission tests for Django 3.2 `.alias()` — non-projected
//! annotation. Issue #268.
//!
//! Acceptance:
//! 1. `.alias(name, expr)` registers the aggregate but the writer **omits
//!    it from the SELECT list**.
//! 2. `.filter(name, ...)` lifts the alias expression into HAVING (same
//!    machinery `.annotate()` uses).
//! 3. `.order_by([(name, desc)])` lifts the alias expression into the
//!    `ORDER BY` clause as an expression (not a bare identifier — bare
//!    identifier would reference a non-existent SELECT column).
//! 4. Pure `.alias()` (no `.annotate()`) still triggers GROUP BY
//!    auto-inference when the aliased expression is aggregating.
//! 5. The SAME `AggregateQuery` produces correct SQL on PG, MySQL, and
//!    SQLite — no hardcoded dialect.

use rustango::core::aggregates::count_all;
use rustango::core::{Model as _, Op};
use rustango::query::QuerySet;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "alias_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    name: String,
    active: bool,
}

fn build_query() -> rustango::core::AggregateQuery {
    QuerySet::<Author>::default()
        .aggregate()
        .group_by("id")
        .alias("c", count_all().into())
        .filter("c", Op::Gt, 5_i64)
        .order_by(&[("c", true)])
        .compile()
        .expect("compile")
}

#[test]
fn alias_omits_from_select_pg() {
    let q = build_query();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // SELECT list contains the group-by column only — no `AS "c"`.
    assert!(
        stmt.sql.starts_with(r#"SELECT "id" FROM "alias_author""#),
        "expected projection without alias, got:\n{}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(r#"AS "c""#),
        "alias must not appear in SELECT projection, got:\n{}",
        stmt.sql
    );
    // Aliased expression still drives HAVING — referenced by full COUNT(*),
    // not by its `c` name (PG strictly disallows SELECT aliases in HAVING
    // anyway).
    assert!(
        stmt.sql.contains("HAVING COUNT(*) > $1"),
        "HAVING must reference the lifted expression, got:\n{}",
        stmt.sql
    );
    // ORDER BY also references the full expression.
    assert!(
        stmt.sql.contains("ORDER BY COUNT(*) DESC"),
        "ORDER BY must reference the lifted expression, got:\n{}",
        stmt.sql
    );
}

#[test]
fn alias_omits_from_select_mysql() {
    let q = build_query();
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.starts_with("SELECT `id` FROM `alias_author`"),
        "expected projection without alias, got:\n{}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("AS `c`"),
        "alias must not appear in SELECT projection, got:\n{}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("HAVING COUNT(*) > ?"),
        "HAVING must reference the lifted expression, got:\n{}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("ORDER BY COUNT(*) DESC"),
        "ORDER BY must reference the lifted expression, got:\n{}",
        stmt.sql
    );
}

#[test]
fn alias_omits_from_select_sqlite() {
    let q = build_query();
    let stmt = Sqlite.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.starts_with(r#"SELECT "id" FROM "alias_author""#),
        "expected projection without alias, got:\n{}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(r#"AS "c""#),
        "alias must not appear in SELECT projection, got:\n{}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("HAVING COUNT(*) > ?"),
        "HAVING must reference the lifted expression, got:\n{}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("ORDER BY COUNT(*) DESC"),
        "ORDER BY must reference the lifted expression, got:\n{}",
        stmt.sql
    );
}

#[test]
fn alias_alongside_annotate_keeps_only_annotate_in_select() {
    // Mixed: `.annotate("n", ...)` projects, `.alias("c", ...)` does not.
    // Both should be filterable.
    let q = QuerySet::<Author>::default()
        .aggregate()
        .group_by("id")
        .annotate("n", count_all().into())
        .alias("c", count_all().into())
        .filter("c", Op::Gt, 0_i64)
        .compile()
        .expect("compile");

    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // Annotate projected with `AS "n"`.
    assert!(
        stmt.sql.contains(r#"COUNT(*) AS "n""#),
        "annotate must project, got:\n{}",
        stmt.sql
    );
    // Alias NOT projected — no `AS "c"`.
    assert!(
        !stmt.sql.contains(r#"AS "c""#),
        "alias must not project, got:\n{}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("HAVING COUNT(*) > $1"),
        "HAVING must lift alias expression, got:\n{}",
        stmt.sql
    );
}

#[test]
fn alias_triggers_group_by_inference_without_annotate() {
    // Pure `.alias()` with aggregating expression and no explicit
    // group_by — auto-inference must still kick in (otherwise SQL is
    // invalid: aggregate in HAVING with no GROUP BY).
    let q = QuerySet::<Author>::default()
        .aggregate()
        .alias("c", count_all().into())
        .filter("c", Op::Gt, 0_i64)
        .compile()
        .expect("compile");

    // Django Shape 3 — auto group by every scalar column.
    assert!(!q.group_by.is_empty(), "expected auto-inferred GROUP BY");
    assert_eq!(q.aliases.len(), 1);
    assert!(q.aggregates.is_empty());
}
