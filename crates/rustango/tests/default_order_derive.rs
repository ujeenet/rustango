//! `#[rustango(default_order = "...")]` — schema-declared default
//! ORDER BY, per-query opt-in. Closes #291 / T2.5.
//!
//! Pins:
//!   1. Default queryset emits **no** ORDER BY (no Django stickiness).
//!   2. `.with_default_order()` opts in to the schema's default.
//!   3. `.order_by(...)` after `.with_default_order()` appends as
//!      secondary sort keys.
//!   4. `.unordered()` clears any prior order entries.
//!   5. `-prefix` parses as descending; bare name parses as ascending.

use rustango::query::QuerySet;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "default_order_post")]
#[rustango(default_order = "-created, +status")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 1)]
    status: String,
    created: chrono::DateTime<chrono::Utc>,
}

fn compile_pg(qs: QuerySet<Post>) -> String {
    let q = qs.compile().unwrap();
    Postgres.compile_select(&q).unwrap().sql
}

#[test]
fn default_queryset_emits_no_order_by() {
    let sql = compile_pg(Post::objects());
    assert!(
        !sql.contains("ORDER BY"),
        "default queryset must NOT pay for sort: {sql}"
    );
}

#[test]
fn with_default_order_applies_schema_default() {
    let sql = compile_pg(Post::objects().with_default_order());
    assert!(
        sql.contains(r#"ORDER BY "created" DESC, "status""#),
        "schema default_order should emit (created DESC, status ASC): {sql}"
    );
}

#[test]
fn order_by_after_default_appends_secondary_keys() {
    let sql = compile_pg(
        Post::objects()
            .with_default_order()
            .order_by(&[("id", false)]),
    );
    assert!(
        sql.contains(r#"ORDER BY "created" DESC, "status", "id""#),
        "default + secondary should append: {sql}"
    );
}

#[test]
fn unordered_clears_default_order() {
    let sql = compile_pg(
        Post::objects()
            .with_default_order()
            .order_by(&[("id", false)])
            .unordered(),
    );
    assert!(!sql.contains("ORDER BY"), "unordered must clear: {sql}");
}

#[test]
fn unordered_then_order_by_emits_only_explicit() {
    let sql = compile_pg(
        Post::objects()
            .with_default_order()
            .unordered()
            .order_by(&[("status", true)]),
    );
    // Slice on the ORDER BY clause specifically — "created" lives in
    // the SELECT projection too, so substring-matching the whole SQL
    // is the wrong gate.
    let order_by_clause = sql.split("ORDER BY").nth(1).unwrap_or("");
    assert!(order_by_clause.contains("\"status\" DESC"));
    assert!(
        !order_by_clause.contains("\"created\""),
        "default keys must be cleared in ORDER BY: {sql}"
    );
}

#[test]
fn with_default_order_is_idempotent() {
    let once = compile_pg(Post::objects().with_default_order());
    let twice = compile_pg(Post::objects().with_default_order().with_default_order());
    assert_eq!(once, twice, "with_default_order must be idempotent");
}

#[test]
fn schema_carries_parsed_default_order() {
    // Direct assertion against the macro-emitted slice. `-prefix` →
    // desc=true; `+prefix` and bare → desc=false.
    use rustango::core::Model as _;
    let schema = Post::SCHEMA;
    assert_eq!(
        schema.default_order,
        &[("created", true), ("status", false)],
    );
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "no_default_post")]
#[allow(dead_code)]
pub struct NoDefault {
    #[rustango(primary_key)]
    id: i64,
}

#[test]
fn with_default_order_is_noop_when_schema_has_none() {
    let sql = compile_pg_no_default(NoDefault::objects().with_default_order());
    assert!(
        !sql.contains("ORDER BY"),
        "no default_order on schema → no ORDER BY even after .with_default_order(): {sql}"
    );
}

fn compile_pg_no_default(qs: QuerySet<NoDefault>) -> String {
    let q = qs.compile().unwrap();
    Postgres.compile_select(&q).unwrap().sql
}
