//! Ad-hoc joins.
//!
//! [`crate::query::QuerySet::select_related`] follows FK edges for you.
//! This module is the escape hatch: join any table on any predicate.
//! The predicate is a plain [`WhereExpr`], the same type `WHERE` uses,
//! so `and()` / `or()` / `Not` / function calls all work in the `ON`
//! clause.
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
//! ## Naming columns inside `on`
//!
//! - Bare `Filter` / `ColumnFilter` columns belong to the joined
//!   alias, the one you passed to `.join(...)`.
//! - `aliased(alias, col)` emits `"<alias>"."<col>"`. Use it to point
//!   back at the outer table or at an earlier join. The outer alias is
//!   the outer model's `table` name.
//! - `WhereExpr::ExprCompare` takes an alias on both sides. Use it for
//!   the column-on-column join condition.
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
//! ## Dialect support
//!
//! `Inner` and `Left` work everywhere. `Right` is PG and MySQL only.
//! `Full` is PG only. The others raise
//! [`SqlError::JoinKindNotSupported`].
//!
//! [`SqlError::JoinKindNotSupported`]: crate::sql::SqlError::JoinKindNotSupported

use super::expr::Expr;
use super::query::{Op, WhereExpr};
use super::SqlValue;

/// Shorthand for [`Expr::AliasedColumn`]. The alias is either a join's
/// alias (the second argument to `.join(...)`) or the outer model's
/// `table` name.
#[must_use]
pub fn aliased(alias: &'static str, column: &'static str) -> Expr {
    Expr::AliasedColumn { alias, column }
}

/// Predicate builder for JOIN `on` clauses. Emits
/// `"<alias>"."<col>" <op> <value>`. Use it whenever the column you
/// filter on does not belong to the join's own alias.
///
/// # Why prefer this over a bare typed filter
///
/// Inside `on`, bare `Filter` columns are qualified with the joined
/// alias. So `Post::status.eq("draft").into()`, where `Post` is the
/// *outer* model, emits `"<joined_alias>"."status"`. The
/// `TypedFilter<Post>` loses its model tag at the `Into<WhereExpr>`
/// boundary, so the compiler cannot catch this.
///
/// `col_filter` sets the alias itself through
/// [`Expr::AliasedColumn`] + [`WhereExpr::ExprCompare`], so the SQL
/// matches the call site:
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
/// Only the comparison ops (`Eq`, `Ne`, `Lt`, `Lte`, `Gt`, `Gte`) make
/// sense here. Anything else fails with
/// [`SqlError::OpNotSupportedInDialect`] when the SQL is written. For
/// `IN` / `BETWEEN` / `IS NULL` on an aliased column, build the
/// `WhereExpr` by hand.
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
