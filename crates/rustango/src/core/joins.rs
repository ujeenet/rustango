//! Ad-hoc joins — issue #80.
//!
//! Where [`crate::core::QuerySet::select_related`] follows FK edges
//! automatically, this module adds a SQLAlchemy-shape escape hatch
//! for joining against an arbitrary table with an arbitrary predicate.
//! The predicate accepts the same [`WhereExpr`] machinery that powers
//! `WHERE` clauses, so `and()` / `or()` / `Not` / function calls /
//! sub-conditions all compose freely inside the JOIN `ON` clause.
//!
//! [`WhereExpr`]: crate::core::WhereExpr
//!
//! ```ignore
//! use rustango::core::joins::aliased;
//! use rustango::core::{JoinKind, Op, WhereExpr};
//!
//! // INNER JOIN comment AS c ON c.post_id = post.id AND c.is_approved = true
//! let on = WhereExpr::And(vec![
//!     WhereExpr::ExprCompare {
//!         lhs: aliased("c", "post_id"),
//!         op: Op::Eq,
//!         rhs: aliased("post", "id"),
//!     },
//!     WhereExpr::Predicate(rustango::core::Filter {
//!         column: "is_approved",
//!         op: Op::Eq,
//!         value: rustango::core::SqlValue::Bool(true),
//!     }),
//! ]);
//! Post::objects()
//!     .join(Comment::SCHEMA, "c", JoinKind::Inner, on)
//!     .fetch(&pool).await?;
//! ```
//!
//! ## Column qualification inside `on`
//!
//! - **Bare `Filter` / `ColumnFilter` columns** resolve to the joined
//!   alias (i.e. the `<alias>` you passed to `.join(...)`). That's
//!   the natural reading when most of the predicate is about the
//!   joined table.
//! - **`aliased(alias, col)`** emits `"<alias>"."<col>"` explicitly,
//!   for cross-references back to the outer table or to a previously
//!   joined alias. Use the outer model's `table` name as the alias
//!   when referring to the outer side.
//! - **`WhereExpr::ExprCompare`** lets both sides carry their own
//!   alias via `aliased(...)`. Use this for the column-on-column
//!   join condition.
//!
//! ## When to reach for ad-hoc joins
//!
//! | Need | Tool |
//! |---|---|
//! | Pull related rows along with the main row (Django shape) | `select_related` |
//! | Filter the main rows by a related-table predicate | `exists(...)` / `not_exists(...)` |
//! | Need both joined columns AND a custom join predicate | `join(...)` |
//! | One-shot anti-join | `not_exists(...)` |
//!
//! ## Dialect-portability
//!
//! - `Inner` / `Left` — every dialect.
//! - `Right` — PG + MySQL only. SQLite raises
//!   [`SqlError::JoinKindNotSupported`].
//! - `Full` — PG only. MySQL + SQLite raise
//!   [`SqlError::JoinKindNotSupported`].
//!
//! [`SqlError::JoinKindNotSupported`]: crate::sql::SqlError::JoinKindNotSupported

use super::expr::Expr;
use super::query::{Op, WhereExpr};
use super::SqlValue;

/// Shorthand for [`Expr::AliasedColumn`]. The alias can be a join's
/// explicit alias (the second arg to `.join(...)`) or the outer
/// model's `table` name when referring back to the outer side.
#[must_use]
pub fn aliased(alias: &'static str, column: &'static str) -> Expr {
    Expr::AliasedColumn { alias, column }
}

/// Safe predicate-builder for JOIN `on` clauses: emits
/// `"<alias>"."<col>" <op> <value>`. Use this whenever filtering
/// inside an `on` predicate against a column whose table isn't the
/// join's default alias.
///
/// # Why prefer this over a bare typed filter
///
/// Inside an `on` predicate, the writer auto-qualifies bare
/// `Filter` columns to the joined alias. If you write
/// `Post::status.eq("draft").into()` — where `Post` is the *outer*
/// model, not the joined one — the writer emits
/// `"<joined_alias>"."status"`, **not** `"<post_table>"."status"`.
/// The `TypedFilter<Post>` loses its model tag at the
/// `Into<WhereExpr>` boundary, so the compiler can't catch this.
///
/// `col_filter` forces an explicit alias on the LHS by routing
/// through [`Expr::AliasedColumn`] + [`WhereExpr::ExprCompare`], so
/// the emitted SQL matches the call site verbatim:
///
/// ```ignore
/// use rustango::core::joins::col_filter;
/// use rustango::core::Op;
///
/// // ON c.post_id = post.id AND post.status = 'draft'
/// let on = WhereExpr::And(vec![
///     WhereExpr::ExprCompare {
///         lhs: aliased("c", "post_id"),
///         op: Op::Eq,
///         rhs: aliased("post", "id"),
///     },
///     // Safe: explicitly qualifies to the outer table.
///     col_filter("post", "status", Op::Eq, "draft"),
/// ]);
/// ```
///
/// Only binary-comparison ops (`Eq`, `Ne`, `Lt`, `Lte`, `Gt`, `Gte`)
/// are meaningful here — other ops surface as
/// [`SqlError::OpNotSupportedInDialect`] at emit time. For `IN` /
/// `BETWEEN` / `IS NULL` against an aliased column, drop into a
/// hand-rolled `WhereExpr` until a richer helper ships.
///
/// [`SqlError::OpNotSupportedInDialect`]: crate::sql::SqlError::OpNotSupportedInDialect
#[must_use]
pub fn col_filter(
    alias: &'static str,
    column: &'static str,
    op: Op,
    value: impl Into<SqlValue>,
) -> WhereExpr {
    WhereExpr::ExprCompare {
        lhs: Expr::AliasedColumn { alias, column },
        op,
        rhs: Expr::Literal(value.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliased_emits_aliased_column_variant() {
        let e = aliased("comments", "post_id");
        assert_eq!(
            e,
            Expr::AliasedColumn {
                alias: "comments",
                column: "post_id",
            },
        );
    }

    #[test]
    fn col_filter_wraps_into_expr_compare_with_aliased_lhs() {
        let w = col_filter("post", "status", Op::Eq, "draft");
        match w {
            WhereExpr::ExprCompare {
                lhs: Expr::AliasedColumn { alias, column },
                op,
                rhs: Expr::Literal(SqlValue::String(s)),
            } => {
                assert_eq!(alias, "post");
                assert_eq!(column, "status");
                assert_eq!(op, Op::Eq);
                assert_eq!(s, "draft");
            }
            _ => panic!("expected ExprCompare with aliased LHS + literal RHS, got {w:?}"),
        }
    }
}
