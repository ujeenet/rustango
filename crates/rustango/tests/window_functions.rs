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
        aggregates: vec![("w".into(), expr)],
        aliases: vec![],
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

// ---------- Window-as-Expr IR-shape (diagnostic) ----------

/// **Diagnostic-only test — DO NOT take as a sanctioned use case.**
///
/// `Expr::Window` is an IR-level construct: the builder implements
/// `Into<Expr>` so the variant slots into recursive composition
/// (e.g. inside `Case` / `Coalesced` / `Subquery`). PG, MySQL 8+, and
/// SQLite 3.25+ all restrict window functions to the SELECT list +
/// ORDER BY clause of a query — they're rejected by every backend
/// rustango supports inside `UPDATE SET`, `WHERE`, `HAVING`,
/// `JOIN ON`, etc.
///
/// This test pins the emission shape for documentation/debugging
/// purposes — it does NOT mean the resulting SQL executes. Users
/// must route window expressions through `annotate()` (the only
/// sanctioned channel today); see the cookbook for the supported
/// shape and the subquery workaround for UPDATE-from-window
/// patterns.
#[test]
fn window_as_expr_emits_when_force_constructed_but_db_will_reject() {
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
        "IR emits — but PG will error at execute with \
         `window functions are not allowed in UPDATE`. \
         Pinning the SQL string for diagnostic visibility, not as \
         a sanctioned use case. Got: {}",
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

// ---------- Paranoid-review regressions ----------

/// Validator regression: a typo'd `partition_by` column inside an
/// `annotate("...", window_fn())` call must surface at `compile()`,
/// not at execute. Pre-fix this slipped through because the
/// `Expr::Window` validator only walks the `Expr` path while
/// `annotate()` lowers to `AggregateExpr::Window`.
#[test]
fn partition_by_typo_inside_annotate_is_caught_at_compile_time() {
    use rustango::core::QueryError;
    let err = User::objects()
        .aggregate()
        .annotate("w", row_number().partition_by("nope_col").into())
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope_col"),
        "expected UnknownField for partition_by typo, got: {err:?}",
    );
}

#[test]
fn order_by_typo_inside_annotate_is_caught_at_compile_time() {
    use rustango::core::QueryError;
    let err = User::objects()
        .aggregate()
        .annotate(
            "w",
            row_number()
                .partition_by("tenant_id")
                .order_by(&[("nope_order", true)])
                .into(),
        )
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope_order"),
        "expected UnknownField for order_by typo, got: {err:?}",
    );
}

#[test]
fn lag_column_arg_typo_inside_annotate_is_caught_at_compile_time() {
    use rustango::core::QueryError;
    let err = User::objects()
        .aggregate()
        .annotate(
            "w",
            lag("nope_arg_col", 1, None)
                .order_by(&[("id", false)])
                .into(),
        )
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope_arg_col"),
        "expected UnknownField for LAG column arg typo, got: {err:?}",
    );
}

/// Same path runs through `Coalesced` and `Filtered` wrappers — the
/// inner Window's column refs must still validate. (Filtered + Window
/// is rejected at emit time, but the column-validation walk runs
/// at compile() before emit, so column typos inside the wrapped
/// Window should still surface here.)
#[test]
fn coalesced_window_partition_typo_still_caught() {
    use rustango::core::QueryError;
    // Build the AggregateExpr directly — there's no `aggregates::*`
    // builder for "Coalesced { Window }" today (Coalesced is a
    // wrapper on the flat builder; this combo lands later).
    let inner: AggregateExpr = row_number().partition_by("nope_col_inside_coalesce").into();
    let wrapped = AggregateExpr::Coalesced {
        inner: Box::new(inner),
        default: SqlValue::I64(0),
    };
    let err = User::objects()
        .aggregate()
        .annotate("w", wrapped)
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope_col_inside_coalesce"),
        "Coalesced wrapper must not hide inner column typos: {err:?}",
    );
}

/// Pinned-behavior test for the `LAST_VALUE` default-frame trap —
/// the cookbook now documents this. A bare `last_value` returns the
/// current-row value, not the partition's last row. The SQL emitted
/// here is the same as before the cookbook callout (no behavior
/// change); this test is documentation-in-code so the next reader
/// sees the bare emission alongside the explicit-frame fix.
#[test]
fn last_value_bare_emits_implicit_default_frame_form() {
    let w = last_value("score").order_by(&[("id", false)]);
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains(r#"LAST_VALUE("score") OVER (ORDER BY "id")"#),
        "got: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("ROWS")
            && !stmt.sql.contains("RANGE")
            && !stmt.sql.contains("UNBOUNDED"),
        "bare last_value emits no explicit frame — DEFAULT frame applies, \
         which returns the CURRENT row's value (cookbook footgun): {}",
        stmt.sql
    );
}

/// And the explicit-frame fix — pin the cookbook-recommended shape
/// so the recommended pattern stays callable.
#[test]
fn last_value_with_unbounded_following_frame_emits_full_form() {
    let w = last_value("score")
        .partition_by("tenant_id")
        .order_by(&[("id", false)])
        .frame(WindowFrame {
            kind: FrameKind::Rows,
            start: FrameBoundary::UnboundedPreceding,
            end: Some(FrameBoundary::UnboundedFollowing),
        });
    let stmt = Postgres.compile_aggregate(&agg(w.into())).unwrap();
    assert!(
        stmt.sql
            .contains("ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING"),
        "explicit unbounded-following frame for last_value: {}",
        stmt.sql
    );
}

/// Round-2 regression: rejecting `Filtered { Window }` must surface
/// the same error wrapper across all three dialects. Pre-fix the PG
/// and SQLite paths returned the unhelpful internal name
/// `"Window at format_bare_aggregate site"` while MySQL returned the
/// clean `"Filtered(Window)"`. The upfront `matches!(inner, Window)`
/// guard in `write_aggregate_expr` now produces the consistent
/// `"Filtered(Window)"` everywhere.
#[test]
fn filtered_window_rejection_msg_consistent_across_dialects() {
    use rustango::core::{Filter, Op, WindowExpr, WindowFn};
    use rustango::sql::{MySql, SqlError, Sqlite};
    let f = AggregateExpr::Filtered {
        inner: Box::new(AggregateExpr::Window(Box::new(WindowExpr {
            kind: WindowFn::RowNumber,
            args: vec![],
            partition_by: vec![],
            order_by: vec![],
            frame: None,
        }))),
        filter: WhereExpr::Predicate(Filter {
            column: "score",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    };
    let q = agg(f);
    for (label, err) in [
        ("pg", Postgres.compile_aggregate(&q).unwrap_err()),
        ("mysql", MySql.compile_aggregate(&q).unwrap_err()),
        ("sqlite", Sqlite.compile_aggregate(&q).unwrap_err()),
    ] {
        assert!(
            matches!(
                err,
                SqlError::NestedAggregateWrapper {
                    wrapper: "Filtered(Window)"
                }
            ),
            "{label}: expected wrapper=Filtered(Window), got {err:?}",
        );
    }
}
