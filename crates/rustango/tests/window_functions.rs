//! Tri-dialect emission tests for window functions (issue #7). The
//! standard `<fn>(args) OVER (PARTITION BY … ORDER BY … [frame])`
//! syntax is SQL-standard and works identically on PG ≥ 9.0, MySQL
//! ≥ 8.0, and SQLite ≥ 3.25 — these tests pin the emitted SQL string
//! and the dialect-shape placeholders for each backend.

use rustango::core::window::{
    dense_rank, first_value, lag, last_value, lead, ntile, rank, row_number,
};
use rustango::core::{
    AggregateExpr, AggregateQuery, Expr, FrameBoundary, FrameKind, Model as _, SqlValue, WhereExpr,
    WindowFrame,
};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "wf_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    tenant_id: i64,
    #[rustango(max_length = 100)]
    name: String,
    score: i64,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn agg(expr: AggregateExpr) -> AggregateQuery {
    AggregateQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
        aggregates: vec![("w", expr)],
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
    }
}

// ---------- Per-function emission ----------

#[test]
fn pg_row_number_with_order_by_emits_standard_form() {
    let w = row_number().order_by(&[("score", true), ("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains(r#"ROW_NUMBER() OVER (ORDER BY "score" DESC, "id") AS "w""#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn rank_partition_and_order_compose() {
    let w = rank()
        .partition_by("tenant_id")
        .order_by(&[("score", true)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains(r#"RANK() OVER (PARTITION BY "tenant_id" ORDER BY "score" DESC) AS "w""#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn dense_rank_no_partition_no_order_emits_empty_over_clause() {
    let w = dense_rank();
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql.contains(r#"DENSE_RANK() OVER () AS "w""#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn ntile_args_embeds_bucket_count_literal() {
    // PG's NTILE requires the bucket count as integer (not bigint).
    // The writer inlines the literal in the SQL rather than binding
    // as a $N param, which would type as bigint and fail PG's
    // function lookup.
    let w = ntile(4).order_by(&[("score", true)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains(r#"NTILE(4) OVER (ORDER BY "score" DESC) AS "w""#),
        "got: {}",
        stmt.sql
    );
    assert!(
        stmt.params.is_empty(),
        "NTILE arg should be inline, not bound: {:?}",
        stmt.params
    );
}

#[test]
fn lag_without_default_emits_two_args() {
    let w = lag("score", 1, None).order_by(&[("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    // Offset is inlined as integer literal (PG requires `integer`,
    // not `bigint`, for LAG's second arg).
    assert!(
        stmt.sql
            .contains(r#"LAG("score", 1) OVER (ORDER BY "id") AS "w""#),
        "got: {}",
        stmt.sql
    );
    assert!(stmt.params.is_empty());
}

#[test]
fn lag_with_default_emits_three_args() {
    let w = lag("score", 2, Some(SqlValue::I64(0))).order_by(&[("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    // Offset inline (2); default bound as a regular bigint param ($1).
    assert!(
        stmt.sql
            .contains(r#"LAG("score", 2, $1) OVER (ORDER BY "id") AS "w""#),
        "got: {}",
        stmt.sql
    );
    assert_eq!(stmt.params, vec![SqlValue::I64(0)]);
}

#[test]
fn lead_emits_lead_keyword() {
    let w = lead("score", 1, None).order_by(&[("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql.contains(r#"LEAD("score", 1) OVER"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn first_value_and_last_value_take_column_arg() {
    let w = first_value("score")
        .partition_by("tenant_id")
        .order_by(&[("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql.contains(
            r#"FIRST_VALUE("score") OVER (PARTITION BY "tenant_id" ORDER BY "id") AS "w""#
        ),
        "got: {}",
        stmt.sql
    );

    let w = last_value("score").order_by(&[("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(stmt.sql.contains(r#"LAST_VALUE("score") OVER"#));
}

// ---------- Frame clause ----------

#[test]
fn rows_between_unbounded_preceding_and_current_row() {
    let w = last_value("score")
        .partition_by("tenant_id")
        .order_by(&[("id", false)])
        .frame(WindowFrame {
            kind: FrameKind::Rows,
            start: FrameBoundary::UnboundedPreceding,
            end: Some(FrameBoundary::CurrentRow),
        });
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains("ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn rows_n_preceding_to_n_following() {
    let w = first_value("score")
        .order_by(&[("id", false)])
        .frame(WindowFrame {
            kind: FrameKind::Rows,
            start: FrameBoundary::Preceding(5),
            end: Some(FrameBoundary::Following(5)),
        });
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains("ROWS BETWEEN 5 PRECEDING AND 5 FOLLOWING"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn range_unbounded_preceding_only_no_end() {
    let w = first_value("score")
        .order_by(&[("id", false)])
        .frame(WindowFrame {
            kind: FrameKind::Range,
            start: FrameBoundary::UnboundedPreceding,
            end: None,
        });
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql.contains("RANGE UNBOUNDED PRECEDING"),
        "got: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("BETWEEN"),
        "single-boundary frames omit BETWEEN: {}",
        stmt.sql
    );
}

// ---------- Tri-dialect ident-quote shapes ----------

#[test]
fn mysql_uses_backticks_for_window_columns() {
    let w = rank()
        .partition_by("tenant_id")
        .order_by(&[("score", true)]);
    let stmt = MySql.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains("RANK() OVER (PARTITION BY `tenant_id` ORDER BY `score` DESC)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_uses_double_quotes_for_window_columns() {
    let w = rank()
        .partition_by("tenant_id")
        .order_by(&[("score", true)]);
    let stmt = Sqlite.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains(r#"RANK() OVER (PARTITION BY "tenant_id" ORDER BY "score" DESC)"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Window-as-Expr (UPDATE set_expr) ----------

#[test]
fn window_as_expr_inside_update_set_emits_full_form() {
    use rustango::core::{Assignment, Filter, Op, UpdateQuery};
    let expr: Expr = row_number().order_by(&[("score", true)]).into();
    let q = UpdateQuery {
        model: User::SCHEMA,
        set: vec![Assignment {
            column: "score",
            value: expr,
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    };
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"SET "score" = ROW_NUMBER() OVER (ORDER BY "score" DESC)"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Multi-column partition + order ----------

#[test]
fn multi_partition_and_multi_order_emit_in_chain_order() {
    let w = rank()
        .partition_by("tenant_id")
        .partition_by("region")
        .order_by(&[("score", true), ("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains(r#"PARTITION BY "tenant_id", "region" ORDER BY "score" DESC, "id""#),
        "got: {}",
        stmt.sql
    );
}
