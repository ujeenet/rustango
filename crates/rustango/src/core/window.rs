//! Window functions — issue #7.
//!
//! Seventh and final slice of the ORM Expression DSL epic. Closes the
//! Django `Window(expression, partition_by=, order_by=, frame=)` gap
//! with 8 function variants + ROWS/RANGE frame clauses. Tri-dialect
//! uniform — every backend rustango supports (PG ≥ 9.0, MySQL ≥ 8.0,
//! SQLite ≥ 3.25) ships native `OVER (…)` syntax.
//!
//! ```ignore
//! use rustango::core::window::{rank, row_number, lag};
//! use rustango::core::SqlValue;
//!
//! // "Rank users by score within each tenant" — the canonical
//! // window-function use case. Lives in the SELECT list via
//! // `annotate()`.
//! User::objects()
//!     .aggregate()
//!     .annotate(
//!         "tenant_rank",
//!         rank().partition_by("tenant_id").order_by(&[("score", true)]),
//!     )
//!     .compile()?;
//!
//! // Lag with COALESCE fallback for the first row of each partition.
//! // Composes naturally because LAG returns NULL for the boundary row
//! // and `aggregates::sum(...).default(...)` shape applies.
//! use rustango::core::aggregates::AggregateBuilder;
//! Event::objects()
//!     .aggregate()
//!     .annotate(
//!         "prev_count",
//!         AggregateBuilder::from(
//!             lag("count", 1, Some(SqlValue::I64(0)))
//!                 .partition_by("user_id")
//!                 .order_by(&[("day", false)]),
//!         ),
//!     )
//!     .compile()?;
//! ```
//!
//! ## Where window functions can appear
//!
//! Every backend rustango supports (PG, MySQL 8+, SQLite 3.25+)
//! restricts window functions to the **SELECT list** and the
//! **ORDER BY clause** of a query. They are **not allowed in**:
//!
//! - `WHERE` predicates,
//! - `HAVING` predicates,
//! - `GROUP BY` clauses,
//! - `UPDATE SET` assignments,
//! - `JOIN ON` predicates,
//! - the projection of a `RETURNING` clause.
//!
//! The IR + writer don't gate emission on this — `set_expr(...,
//! row_number())` compiles cleanly but fails at execute. Build window
//! expressions through [`crate::query::AggregateBuilder::annotate`]
//! (the only legitimate channel today). To use a window result inside
//! an `UPDATE` or filter, wrap the windowed select in a subquery and
//! join/filter against that.
//!
//! ## Builder shape
//!
//! Each constructor (`row_number`, `rank`, `dense_rank`, `lag`,
//! `lead`, `first_value`, `last_value`, `ntile`) returns a
//! [`WindowBuilder`] with three chainable modifiers:
//!
//! - `.partition_by(col)` — append a `PARTITION BY` column. Call
//!   multiple times for multi-column partitioning.
//! - `.order_by(&[("col", desc)])` — append `ORDER BY` columns.
//! - `.frame(WindowFrame { … })` — set the optional `ROWS`/`RANGE`
//!   frame clause.
//!
//! The builder lowers via `Into<AggregateExpr>` so window functions
//! compose with `annotate()`. `Into<Expr>` is also implemented to
//! keep the IR composable (window-in-Case/Coalesce/Subquery), but
//! emitting one in a slot the DB rejects (see list above) is a
//! programmer error.
//!
//! ## Tri-dialect emission
//!
//! `<fn>(args) OVER (PARTITION BY … ORDER BY … [frame])` is SQL-standard
//! syntax. Identical SQL across PG / MySQL 8+ / SQLite 3.25+.

use super::expr::Expr;
use super::query::{AggregateExpr, OrderClause};
use super::SqlValue;

/// Window function kind. The eight v1 variants from Django's epic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFn {
    /// `ROW_NUMBER()` — sequential row index within the partition.
    RowNumber,
    /// `RANK()` — rank within partition with gaps on ties.
    Rank,
    /// `DENSE_RANK()` — rank within partition without gaps.
    DenseRank,
    /// `NTILE(buckets)` — divide partition into `buckets` groups.
    Ntile,
    /// `LAG(value, offset, default)` — value from the row `offset`
    /// rows before the current row in the partition.
    Lag,
    /// `LEAD(value, offset, default)` — value from the row `offset`
    /// rows after the current row in the partition.
    Lead,
    /// `FIRST_VALUE(expr)` — value from the first row in the
    /// partition/frame.
    FirstValue,
    /// `LAST_VALUE(expr)` — value from the last row in the
    /// partition/frame.
    LastValue,
}

/// `ROWS` vs `RANGE` frame mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `ROWS BETWEEN …` — physical row offsets.
    Rows,
    /// `RANGE BETWEEN …` — logical value ranges relative to the
    /// current row's ORDER BY key.
    Range,
}

/// One end of a frame range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBoundary {
    /// `UNBOUNDED PRECEDING`.
    UnboundedPreceding,
    /// `<n> PRECEDING`.
    Preceding(i64),
    /// `CURRENT ROW`.
    CurrentRow,
    /// `<n> FOLLOWING`.
    Following(i64),
    /// `UNBOUNDED FOLLOWING`.
    UnboundedFollowing,
}

/// Frame specification — `ROWS|RANGE BETWEEN <start> AND <end>`
/// (when `end` is `Some`) or just `ROWS|RANGE <start>` (when `end`
/// is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowFrame {
    pub kind: FrameKind,
    pub start: FrameBoundary,
    pub end: Option<FrameBoundary>,
}

/// Window-expression IR. Carries the function kind, its arguments
/// (e.g. `LAG`'s expr/offset/default), and the OVER clause.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowExpr {
    pub kind: WindowFn,
    pub args: Vec<Expr>,
    pub partition_by: Vec<&'static str>,
    pub order_by: Vec<OrderClause>,
    pub frame: Option<WindowFrame>,
}

/// Fluent wrapper around a [`WindowExpr`]. Built via the free
/// functions in this module ([`row_number`], [`rank`], …); finalize
/// by passing into anything that takes `impl Into<Expr>` or
/// `impl Into<AggregateExpr>`.
#[must_use]
pub struct WindowBuilder {
    inner: WindowExpr,
}

impl WindowBuilder {
    fn new(kind: WindowFn, args: Vec<Expr>) -> Self {
        Self {
            inner: WindowExpr {
                kind,
                args,
                partition_by: Vec::new(),
                order_by: Vec::new(),
                frame: None,
            },
        }
    }

    /// Append a `PARTITION BY` column. Call multiple times to
    /// partition by multiple columns (left-to-right precedence).
    pub fn partition_by(mut self, column: &'static str) -> Self {
        self.inner.partition_by.push(column);
        self
    }

    /// Append `ORDER BY` columns. `desc = true` → `DESC`. Multiple
    /// calls compose; subsequent ones append after earlier ones.
    pub fn order_by(mut self, items: &[(&'static str, bool)]) -> Self {
        for (col, desc) in items {
            self.inner.order_by.push(OrderClause {
                column: col,
                desc: *desc,
            });
        }
        self
    }

    /// Set the frame clause. Replaces any previous frame.
    pub fn frame(mut self, frame: WindowFrame) -> Self {
        self.inner.frame = Some(frame);
        self
    }

    /// Finalize to an [`Expr`]. Equivalent to `Into<Expr>::into(b)` —
    /// provided when type inference needs help.
    #[must_use]
    pub fn build(self) -> Expr {
        Expr::Window(Box::new(self.inner))
    }
}

impl From<WindowBuilder> for Expr {
    fn from(b: WindowBuilder) -> Self {
        b.build()
    }
}

impl From<WindowBuilder> for AggregateExpr {
    fn from(b: WindowBuilder) -> Self {
        AggregateExpr::Window(Box::new(b.inner))
    }
}

// ---------------------------------------------------------------- builders

/// `ROW_NUMBER() OVER (…)` — sequential 1-based row index within
/// the partition. Ordering is required for deterministic output.
#[must_use]
pub fn row_number() -> WindowBuilder {
    WindowBuilder::new(WindowFn::RowNumber, vec![])
}

/// `RANK() OVER (…)` — rank within partition; tied rows share a
/// rank and the next row's rank skips ahead (e.g., 1, 2, 2, 4).
#[must_use]
pub fn rank() -> WindowBuilder {
    WindowBuilder::new(WindowFn::Rank, vec![])
}

/// `DENSE_RANK() OVER (…)` — rank within partition; tied rows share
/// a rank but the next row's rank doesn't skip (e.g., 1, 2, 2, 3).
#[must_use]
pub fn dense_rank() -> WindowBuilder {
    WindowBuilder::new(WindowFn::DenseRank, vec![])
}

/// `NTILE(buckets) OVER (…)` — divide the partition's rows into
/// `buckets` approximately-equal groups.
#[must_use]
pub fn ntile(buckets: i64) -> WindowBuilder {
    WindowBuilder::new(WindowFn::Ntile, vec![Expr::Literal(SqlValue::I64(buckets))])
}

/// `LAG(<column>, <offset>, <default>) OVER (…)` — value from the
/// row `offset` rows before the current row, with `default`
/// substituted when out of range.
///
/// Pass `offset = 1` for "previous row," `offset = N` for further
/// back. `default = None` produces `LAG(col, offset)` (omits the
/// default arg — NULL is returned for out-of-range positions).
#[must_use]
pub fn lag(column: &'static str, offset: i64, default: Option<SqlValue>) -> WindowBuilder {
    let mut args = vec![Expr::Column(column), Expr::Literal(SqlValue::I64(offset))];
    if let Some(d) = default {
        args.push(Expr::Literal(d));
    }
    WindowBuilder::new(WindowFn::Lag, args)
}

/// `LEAD(<column>, <offset>, <default>) OVER (…)` — value from the
/// row `offset` rows after the current row.
#[must_use]
pub fn lead(column: &'static str, offset: i64, default: Option<SqlValue>) -> WindowBuilder {
    let mut args = vec![Expr::Column(column), Expr::Literal(SqlValue::I64(offset))];
    if let Some(d) = default {
        args.push(Expr::Literal(d));
    }
    WindowBuilder::new(WindowFn::Lead, args)
}

/// `FIRST_VALUE(<column>) OVER (…)` — value of `column` from the
/// first row of the partition/frame.
#[must_use]
pub fn first_value(column: &'static str) -> WindowBuilder {
    WindowBuilder::new(WindowFn::FirstValue, vec![Expr::Column(column)])
}

/// `LAST_VALUE(<column>) OVER (…)` — value of `column` from the
/// last row of the partition/frame.
#[must_use]
pub fn last_value(column: &'static str) -> WindowBuilder {
    WindowBuilder::new(WindowFn::LastValue, vec![Expr::Column(column)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_number_has_no_args() {
        let b = row_number();
        assert!(b.inner.args.is_empty());
        assert_eq!(b.inner.kind, WindowFn::RowNumber);
    }

    #[test]
    fn lag_default_none_omits_default_arg() {
        let b = lag("price", 1, None);
        assert_eq!(b.inner.args.len(), 2);
    }

    #[test]
    fn lag_default_some_includes_default_arg() {
        let b = lag("price", 1, Some(SqlValue::I64(0)));
        assert_eq!(b.inner.args.len(), 3);
    }

    #[test]
    fn partition_by_appends() {
        let b = row_number()
            .partition_by("tenant_id")
            .partition_by("region");
        assert_eq!(b.inner.partition_by, vec!["tenant_id", "region"]);
    }

    #[test]
    fn order_by_appends_multiple() {
        let b = row_number().order_by(&[("score", true), ("id", false)]);
        assert_eq!(b.inner.order_by.len(), 2);
        assert_eq!(b.inner.order_by[0].column, "score");
        assert!(b.inner.order_by[0].desc);
        assert!(!b.inner.order_by[1].desc);
    }

    #[test]
    fn frame_replaces_prior() {
        let f1 = WindowFrame {
            kind: FrameKind::Rows,
            start: FrameBoundary::UnboundedPreceding,
            end: Some(FrameBoundary::CurrentRow),
        };
        let f2 = WindowFrame {
            kind: FrameKind::Range,
            start: FrameBoundary::Preceding(5),
            end: Some(FrameBoundary::Following(5)),
        };
        let b = row_number().frame(f1).frame(f2.clone());
        assert_eq!(b.inner.frame, Some(f2));
    }

    #[test]
    fn lowers_to_expr_window_variant() {
        let e: Expr = row_number().into();
        assert!(matches!(e, Expr::Window(_)));
    }

    #[test]
    fn lowers_to_aggregate_expr_window_variant() {
        let a: AggregateExpr = rank().partition_by("tenant_id").into();
        assert!(matches!(a, AggregateExpr::Window(_)));
    }
}
