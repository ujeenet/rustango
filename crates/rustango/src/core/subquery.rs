//! Subquery / Exists / OuterRef builders (issue #5).
//!
//! The fifth slice of the ORM Expression DSL epic. Three Django-shape
//! primitives that turn a [`SelectQuery`] into something embeddable
//! inside a larger queryset.
//!
//! [`SelectQuery`]: crate::core::SelectQuery
//!
//! ```ignore
//! use rustango::core::subquery::{exists, not_exists, in_subquery, outer_ref};
//! use rustango::core::{Column as _, F};
//!
//! // EXISTS — "authors who have at least one book".
//! let with_books = Book::objects()
//!     .where_(Book::author_id.eq_expr(outer_ref("id")))
//!     .compile()?;
//! let authors = Author::objects()
//!     .where_expr(exists(with_books))
//!     .fetch(&pool).await?;
//!
//! // NOT EXISTS — "authors with no books".
//! let no_books = Book::objects()
//!     .where_(Book::author_id.eq_expr(outer_ref("id")))
//!     .compile()?;
//! let empty = Author::objects()
//!     .where_expr(not_exists(no_books))
//!     .fetch(&pool).await?;
//!
//! // IN (SELECT …) — "posts in any of the public categories".
//! let public_cat_ids = Category::objects()
//!     .where_(Category::is_public.eq(true))
//!     .compile()?;
//! let visible = Post::objects()
//!     .where_expr(in_subquery("category_id", public_cat_ids))
//!     .fetch(&pool).await?;
//! ```
//!
//! ## How OuterRef resolves
//!
//! [`outer_ref("col")`][outer_ref] returns an [`Expr::OuterRef`] that
//! the SQL writer resolves against the immediately enclosing query at
//! emit time. Concretely:
//!
//! ```text
//! SELECT … FROM "author" WHERE EXISTS (
//!     SELECT … FROM "book" WHERE "book"."author_id" = "author"."id"
//!                                                       ^^^^^^^^
//!                                                       OuterRef
//! )
//! ```
//!
//! The writer threads a scope stack through emission — every `EXISTS`,
//! `NOT EXISTS`, `IN (SELECT …)`, and scalar [`subquery`] pushes a
//! frame, and `outer_ref("col")` reads the immediate parent. Multi-
//! level correlation works the same way (parent of parent of …).
//!
//! ## Compile-time validation lives on the inner queryset
//!
//! These builders take an already-compiled [`SelectQuery`], so any
//! column-name typo or schema mismatch surfaces at the inner
//! `queryset.compile()` call — not when the outer query is finally
//! executed. Build the subquery first, propagate `?`, then embed.

use super::expr::Expr;
use super::query::{SelectQuery, WhereExpr};

/// `EXISTS (subquery)` — true when the subquery returns at least one
/// row. Mirrors Django's [`Exists`] expression and is by far the most
/// common subquery shape in ORM code.
///
/// [`Exists`]: https://docs.djangoproject.com/en/6.0/ref/models/expressions/#django.db.models.Exists
#[must_use]
pub fn exists(subquery: SelectQuery) -> WhereExpr {
    WhereExpr::Exists(Box::new(subquery))
}

/// `NOT EXISTS (subquery)` — true when the subquery returns no rows.
/// Django's `~Exists(…)` shorthand. The canonical "find rows in A
/// with no related row in B" pattern.
#[must_use]
pub fn not_exists(subquery: SelectQuery) -> WhereExpr {
    WhereExpr::NotExists(Box::new(subquery))
}

/// `<column> IN (subquery)` — the standard subquery-membership shape.
/// Useful when the inner query needs joins or aggregation that can't
/// be expressed as a flat [`crate::core::Op::In`] list literal.
///
/// `column` is the outer column to compare; `subquery` should select
/// a single column whose values are checked against `column`.
#[must_use]
pub fn in_subquery(column: &'static str, subquery: SelectQuery) -> WhereExpr {
    WhereExpr::InSubquery {
        column,
        negated: false,
        subquery: Box::new(subquery),
    }
}

/// `<column> NOT IN (subquery)` — inverse of [`in_subquery`].
#[must_use]
pub fn not_in_subquery(column: &'static str, subquery: SelectQuery) -> WhereExpr {
    WhereExpr::InSubquery {
        column,
        negated: true,
        subquery: Box::new(subquery),
    }
}

/// Scalar subquery — `(SELECT … FROM …)`. Embeddable as an `Expr`
/// anywhere `set_expr` / `eq_expr` / a CASE THEN slot expects a value.
/// Caller is responsible for shaping the inner queryset (`.limit(1)`,
/// projection narrowing, etc.) so the result is one column × one row;
/// otherwise the database errors at runtime.
#[must_use]
pub fn subquery(inner: SelectQuery) -> Expr {
    Expr::Subquery(Box::new(inner))
}

/// `OuterRef("col")` — reference a column from the enclosing query
/// inside a correlated subquery. Equivalent to Django's
/// [`OuterRef('col')`]. Only resolves correctly when emitted from
/// inside a subquery wrapper ([`exists`], [`not_exists`],
/// [`in_subquery`], [`subquery`]); the writer raises
/// [`crate::sql::SqlError::OuterRefOutsideSubquery`] if it shows up
/// outside one.
///
/// `column` is a column name on the outer model — the writer
/// qualifies it as `"<outer_table>"."<col>"` at emission time, so the
/// generated SQL stays unambiguous even with name collisions across
/// inner and outer tables.
///
/// [`OuterRef('col')`]: https://docs.djangoproject.com/en/6.0/ref/models/expressions/#django.db.models.OuterRef
#[must_use]
pub fn outer_ref(column: &'static str) -> Expr {
    Expr::OuterRef(column)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The SelectQuery-shaped builders (`exists`, `not_exists`,
    // `in_subquery`, `not_in_subquery`, `subquery`) are exercised
    // end-to-end by `tests/subquery_expressions.rs` and
    // `tests/subquery_expressions_live.rs` where they can use a real
    // `Model`-derived schema. The unit suite here covers the only
    // builder that doesn't need one.

    #[test]
    fn outer_ref_stores_column_name() {
        let e = outer_ref("id");
        assert_eq!(e, Expr::OuterRef("id"));
    }
}
