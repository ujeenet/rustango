//! Tri-dialect parity for `.values().annotate()` — closes #292 / T2.8.
//!
//! The GROUP BY auto-inference rules (issue #75) are already exercised
//! by `tests/groupby_inference.rs` on Postgres alone. This file
//! confirms the **same** IR produces structurally-correct SQL on
//! Postgres, MySQL, and SQLite, plus that filtering by an `.annotate()`
//! alias lands in HAVING (not WHERE) on every backend.
//!
//! Closes the audit gap T2.8 asked for: the inference path was
//! tight but **undocumented across backends**.

use rustango::core::aggregates::{count_all, sum};
use rustango::core::Op;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "vap_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    author_id: i64,
    #[rustango(max_length = 20)]
    status: String,
    views: i64,
    revenue: i64,
}

// ---------- Django Shape 2: `.values(cols).annotate(agg)` → GROUP BY cols ----------

#[test]
fn shape2_emits_group_by_on_every_dialect() {
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let pg = Postgres.compile_aggregate(&q).unwrap().sql;
    let my = MySql.compile_aggregate(&q).unwrap().sql;
    let sq = Sqlite.compile_aggregate(&q).unwrap().sql;
    // Projection: just the group-by column + aggregate alias.
    assert!(
        pg.contains(r#"SELECT "author_id", COUNT(*) AS "n""#),
        "PG: {pg}"
    );
    assert!(
        my.contains("SELECT `author_id`, COUNT(*) AS `n`"),
        "MySQL: {my}"
    );
    assert!(
        sq.contains(r#"SELECT "author_id", COUNT(*) AS "n""#),
        "SQLite: {sq}"
    );
    // GROUP BY clause present on all three.
    assert!(pg.contains(r#"GROUP BY "author_id""#), "PG: {pg}");
    assert!(my.contains("GROUP BY `author_id`"), "MySQL: {my}");
    assert!(sq.contains(r#"GROUP BY "author_id""#), "SQLite: {sq}");
}

#[test]
fn shape2_multi_column_group_by() {
    let q = Post::objects()
        .values(&["author_id", "status"])
        .annotate("total", sum("revenue").into())
        .compile()
        .unwrap();
    let pg = Postgres.compile_aggregate(&q).unwrap().sql;
    let sq = Sqlite.compile_aggregate(&q).unwrap().sql;
    assert!(pg.contains(r#"GROUP BY "author_id", "status""#), "PG: {pg}");
    assert!(
        sq.contains(r#"GROUP BY "author_id", "status""#),
        "SQLite: {sq}"
    );
}

// ---------- Django Shape 3: `.annotate(agg)` alone → GROUP BY every scalar column ----------

#[test]
fn shape3_emits_group_by_every_scalar_column_on_every_dialect() {
    // Author-shape "every column on the model PLUS the aggregate".
    let q = Post::objects()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let pg = Postgres.compile_aggregate(&q).unwrap().sql;
    let my = MySql.compile_aggregate(&q).unwrap().sql;
    let sq = Sqlite.compile_aggregate(&q).unwrap().sql;
    // GROUP BY contains every scalar column.
    for col in &["id", "author_id", "status", "views", "revenue"] {
        assert!(
            pg.contains(&format!(r#""{col}""#)),
            "PG must include {col}: {pg}"
        );
        assert!(
            my.contains(&format!("`{col}`")),
            "MySQL must include {col}: {my}"
        );
        assert!(
            sq.contains(&format!(r#""{col}""#)),
            "SQLite must include {col}: {sq}"
        );
    }
}

// ---------- HAVING routing: filtering by an annotate alias → HAVING ----------

#[test]
fn filter_on_annotate_alias_routes_to_having() {
    // Authors with > 5 posts.
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .filter("n", Op::Gt, 5_i64)
        .compile()
        .unwrap();
    let pg = Postgres.compile_aggregate(&q).unwrap().sql;
    let my = MySql.compile_aggregate(&q).unwrap().sql;
    let sq = Sqlite.compile_aggregate(&q).unwrap().sql;
    // PG strictly requires the aggregate expression (not the alias) in
    // HAVING. The framework lifts COUNT(*) > $1 into HAVING on every
    // backend for uniformity.
    assert!(pg.contains("HAVING COUNT(*) > $1"), "PG: {pg}");
    assert!(my.contains("HAVING COUNT(*) > ?"), "MySQL: {my}");
    assert!(sq.contains("HAVING COUNT(*) > ?"), "SQLite: {sq}");
}

#[test]
fn filter_on_model_column_routes_to_where() {
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .filter("status", Op::Eq, "published")
        .compile()
        .unwrap();
    let sql = Postgres.compile_aggregate(&q).unwrap().sql;
    assert!(
        sql.contains(r#"WHERE "status" = $1"#),
        "model-column filter must route to WHERE: {sql}"
    );
    // Should NOT land the model column in HAVING.
    assert!(
        !sql.contains(r#"HAVING "status""#),
        "WHERE-clause filter must not appear in HAVING: {sql}"
    );
}

// ---------- Mixed WHERE + HAVING ----------

#[test]
fn mixed_filters_route_to_both_where_and_having() {
    // "Published-post counts per author, where count > 5"
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .filter("status", Op::Eq, "published") // → WHERE
        .filter("n", Op::Gt, 5_i64) // → HAVING (alias)
        .compile()
        .unwrap();
    let pg = Postgres.compile_aggregate(&q).unwrap().sql;
    let my = MySql.compile_aggregate(&q).unwrap().sql;
    let sq = Sqlite.compile_aggregate(&q).unwrap().sql;
    // Both clauses appear on every backend, in the right order
    // (WHERE before GROUP BY, HAVING after).
    for (name, sql) in [("PG", &pg), ("MySQL", &my), ("SQLite", &sq)] {
        let where_pos = sql
            .find("WHERE")
            .unwrap_or_else(|| panic!("{name}: no WHERE: {sql}"));
        let group_pos = sql
            .find("GROUP BY")
            .unwrap_or_else(|| panic!("{name}: no GROUP BY: {sql}"));
        let having_pos = sql
            .find("HAVING")
            .unwrap_or_else(|| panic!("{name}: no HAVING: {sql}"));
        assert!(
            where_pos < group_pos,
            "{name}: WHERE must precede GROUP BY: {sql}"
        );
        assert!(
            group_pos < having_pos,
            "{name}: GROUP BY must precede HAVING: {sql}"
        );
    }
}

// ---------- Bare projection: .values() alone (no annotation) ----------

#[test]
fn bare_values_emits_no_group_by_no_aggregate() {
    // `.values()` without an aggregating `.annotate()` is a pure
    // projection (Django Shape 1) — no GROUP BY emitted.
    use rustango::query::QuerySet;
    let qs: QuerySet<Post> = QuerySet::default();
    let q = qs.values_dict(&["author_id", "status"]).compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.starts_with(r#"SELECT "author_id", "status""#),
        "pure projection: {sql}"
    );
    assert!(
        !sql.contains("GROUP BY"),
        "no GROUP BY in pure projection: {sql}"
    );
    assert!(!sql.contains("COUNT"), "no aggregate: {sql}");
}
