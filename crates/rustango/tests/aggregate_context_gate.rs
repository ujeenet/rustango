//! Issue #88 — the writer's context gate for `Expr::Aggregate`.
//!
//! `Expr::Aggregate(...)` is composable into any Expr slot at the type
//! level, but every SQL backend rejects aggregate calls in
//! WHERE / UPDATE-SET / JOIN-ON / GROUP-BY / RETURNING / non-aggregating
//! SELECT projections. The writer surfaces the error upfront with
//! `SqlError::AggregateOutsideAggregateContext` rather than passing
//! through to the DB.
//!
//! These tests pin the contract on both sides:
//!   1. allowed slots (HAVING, aggregating ORDER BY) still emit cleanly
//!   2. forbidden slots (UPDATE SET, plain WHERE, subquery WHERE, etc.)
//!      surface the typed error before SQL leaves the writer
//!
//! Tri-dialect: the gate is enforced equally across PG / MySQL /
//! SQLite — the underlying SQL standard rejection is universal.

use rustango::core::aggregates::count_all;
use rustango::core::{Column as _, Expr, Op, OrderItem};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "acg_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    pub author_id: i64,
    #[rustango(max_length = 20)]
    pub status: String,
    pub views: i64,
}

/// Build an aggregate IR expression directly — bypassing the public
/// builder so we can drop the bare `Expr::Aggregate` into normally-
/// forbidden slots.
fn count_aggregate_expr() -> Expr {
    Expr::Aggregate(Box::new(rustango::core::AggregateExpr::Count(None)))
}

// ---------- allowed: HAVING predicate (regression for #74) ----------

#[test]
fn aggregate_in_having_compiles_on_every_dialect() {
    // Sanity: the existing #74 HAVING path still emits clean SQL after
    // the gate landed. `filter("alias", Op::Gt, …)` after aggregating
    // `.annotate("c", …)` routes to HAVING through the builder's
    // annotation-alias detection.
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("c", count_all().into())
        .filter("c", Op::Gt, 5_i64)
        .compile()
        .unwrap();
    for d in &[
        &Postgres as &dyn Dialect,
        &MySql as &dyn Dialect,
        &Sqlite as &dyn Dialect,
    ] {
        let stmt = d.compile_aggregate(&q).expect("HAVING emit should succeed");
        assert!(
            stmt.sql.contains(" HAVING "),
            "{}: expected HAVING in {}",
            d.name(),
            stmt.sql
        );
    }
}

// ---------- allowed: ORDER BY on an aggregating query ----------

#[test]
fn aggregate_in_aggregating_order_by_compiles() {
    // `ORDER BY COUNT(*) DESC` is valid SQL when the query aggregates.
    // The gate flips on around the aggregating query's ORDER BY emit.
    let mut q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("c", count_all().into())
        .compile()
        .unwrap();
    q.order_by = vec![OrderItem::expr(count_aggregate_expr(), true)];

    for d in &[
        &Postgres as &dyn Dialect,
        &MySql as &dyn Dialect,
        &Sqlite as &dyn Dialect,
    ] {
        let stmt = d
            .compile_aggregate(&q)
            .expect("aggregating ORDER BY with an aggregate Expr should compile");
        assert!(
            stmt.sql.contains(" ORDER BY "),
            "{}: expected ORDER BY in {}",
            d.name(),
            stmt.sql
        );
        assert!(
            stmt.sql.contains("COUNT(*)"),
            "{}: expected COUNT(*) in ORDER BY position: {}",
            d.name(),
            stmt.sql
        );
    }
}

// ---------- forbidden: UPDATE SET ----------

#[test]
fn aggregate_in_update_set_value_is_rejected() {
    // `UPDATE t SET col = COUNT(*) WHERE ...` — every backend rejects
    // this; we catch it upfront.
    let q = Post::objects()
        .update()
        .set_expr("views", count_aggregate_expr())
        .compile()
        .unwrap();
    for d in &[
        &Postgres as &dyn Dialect,
        &MySql as &dyn Dialect,
        &Sqlite as &dyn Dialect,
    ] {
        let r = d.compile_update(&q);
        assert!(
            matches!(r, Err(SqlError::AggregateOutsideAggregateContext)),
            "{}: expected AggregateOutsideAggregateContext, got {:?}",
            d.name(),
            r
        );
    }
}

// ---------- forbidden: plain WHERE ----------

#[test]
fn aggregate_in_select_where_is_rejected() {
    // `SELECT … FROM t WHERE COUNT(*) > 5` — invalid; aggregates
    // belong in HAVING, not WHERE. Same rejection on every backend.
    let q = Post::objects()
        .where_(Post::views.eq_expr(count_aggregate_expr()))
        .compile()
        .unwrap();
    for d in &[
        &Postgres as &dyn Dialect,
        &MySql as &dyn Dialect,
        &Sqlite as &dyn Dialect,
    ] {
        let r = d.compile_select(&q);
        assert!(
            matches!(r, Err(SqlError::AggregateOutsideAggregateContext)),
            "{}: expected AggregateOutsideAggregateContext, got {:?}",
            d.name(),
            r
        );
    }
}

// ---------- forbidden: AggregateQuery's WHERE (pre-GROUP-BY filter) ----------

#[test]
fn aggregate_in_aggregate_query_where_is_rejected() {
    // Aggregating queries also use WHERE for pre-grouping row filters
    // — aggregates there are illegal (same SQL-standard reason).
    // The gate stays off for WHERE even when the surrounding query
    // aggregates.
    let mut q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("c", count_all().into())
        .compile()
        .unwrap();
    q.where_clause = rustango::core::WhereExpr::Predicate(rustango::core::Filter {
        column: "views",
        op: Op::Eq,
        value: rustango::core::SqlValue::Null,
    });
    // Stash an aggregate inside the WHERE via a ColumnCompare predicate.
    q.where_clause = rustango::core::WhereExpr::ColumnCompare(rustango::core::ColumnFilter {
        column: "views",
        op: Op::Eq,
        rhs: count_aggregate_expr(),
    });
    for d in &[
        &Postgres as &dyn Dialect,
        &MySql as &dyn Dialect,
        &Sqlite as &dyn Dialect,
    ] {
        let r = d.compile_aggregate(&q);
        assert!(
            matches!(r, Err(SqlError::AggregateOutsideAggregateContext)),
            "{}: expected AggregateOutsideAggregateContext, got {:?}",
            d.name(),
            r
        );
    }
}

// ---------- forbidden: ORDER BY of a non-aggregating SELECT ----------

#[test]
fn aggregate_in_plain_select_order_by_is_rejected() {
    // `SELECT … FROM t ORDER BY COUNT(*) DESC` without GROUP BY /
    // aggregating annotation is also rejected by every backend.
    let mut q = Post::objects().compile().unwrap();
    q.order_by = vec![OrderItem::expr(count_aggregate_expr(), true)];

    for d in &[
        &Postgres as &dyn Dialect,
        &MySql as &dyn Dialect,
        &Sqlite as &dyn Dialect,
    ] {
        let r = d.compile_select(&q);
        assert!(
            matches!(r, Err(SqlError::AggregateOutsideAggregateContext)),
            "{}: expected AggregateOutsideAggregateContext, got {:?}",
            d.name(),
            r
        );
    }
}

// ---------- gate doesn't leak into a subquery ----------

#[test]
fn aggregate_in_subquery_where_is_rejected_even_inside_having() {
    // Build a HAVING predicate whose RHS is a correlated subquery
    // whose WHERE contains an `Expr::Aggregate`. The outer HAVING
    // toggles the gate on for ITS body — but crossing into the
    // subquery must restore it to off, so the inner WHERE's
    // aggregate is correctly refused.
    let inner = Post::objects()
        .where_(Post::views.eq_expr(count_aggregate_expr()))
        .compile()
        .unwrap();
    let mut q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("c", count_all().into())
        .compile()
        .unwrap();
    // Hand-build a HAVING that references the subquery — the only
    // way to be sure the gate's stack discipline holds across the
    // SELECT-subquery boundary.
    q.having = Some(rustango::core::WhereExpr::ColumnCompare(
        rustango::core::ColumnFilter {
            column: "c",
            op: Op::Eq,
            rhs: Expr::Subquery(Box::new(inner)),
        },
    ));
    for d in &[
        &Postgres as &dyn Dialect,
        &MySql as &dyn Dialect,
        &Sqlite as &dyn Dialect,
    ] {
        let r = d.compile_aggregate(&q);
        assert!(
            matches!(r, Err(SqlError::AggregateOutsideAggregateContext)),
            "{}: subquery WHERE inside HAVING should still reject aggregate, got {:?}",
            d.name(),
            r
        );
    }
}
