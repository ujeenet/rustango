//! `CASE WHEN … THEN … ELSE … END` conditional expressions.
//!
//! Produces a [`crate::core::Expr`] for `set_expr` and other
//! expression slots.
//!
//! ```ignore
//! use rustango::core::{case::case, F, funcs::lower};
//!
//! // Custom ordering — derive a rank column with CASE, then sort by
//! // it. Today this is the `update().set_expr(...) + order_by(rank)`
//! // pattern; once issue #75 (annotate-with-Expr) lands, the rank can
//! // be computed inline as a SELECT-side annotation instead of
//! // materialized into the column.
//! Post::objects()
//!     .update()
//!     .set_expr(
//!         "priority",
//!         case()
//!             .when(Post::status.eq("published"), 0_i64)
//!             .when(Post::status.eq("draft"), 1_i64)
//!             .default(2_i64),
//!     )
//!     .execute(&pool).await?;
//! let ranked = Post::objects()
//!     .order_by(&[("priority", false), ("id", false)])
//!     .fetch(&pool).await?;
//!
//! // Computed default on update — fall back to lowercased title when
//! // slug is blank.
//! Post::objects()
//!     .update()
//!     .set_expr(
//!         "slug",
//!         case()
//!             .when(Post::slug.eq(""), lower(F("title")))
//!             .default(F("slug")),
//!     )
//!     .execute(&pool).await?;
//! ```
//!
//! Notes:
//!
//! - A `WHEN` condition is any [`WhereExpr`], the same shape as a
//!   `where_()` clause: `Column::eq()`, `.and()`, `.or()`, `Not(...)`.
//! - `.default(...)` is optional. Without it, a `CASE` that matches no
//!   branch returns `NULL`.
//! - The syntax is standard SQL, so PG, MySQL and SQLite all get the
//!   same string.
//!
//! [`WhereExpr`]: crate::core::WhereExpr

use super::expr::{CaseBranch, Expr};
use super::query::WhereExpr;

/// Builder for an [`Expr::Case`]. Start with [`case()`] and pass the
/// result to any `impl Into<Expr>` slot, such as `set_expr`. It builds
/// even with no branches, but the writer then returns
/// `SqlError::EmptyCaseBranches` before the database sees the query.
#[must_use]
pub struct CaseBuilder {
    branches: Vec<CaseBranch>,
    default: Option<Box<Expr>>,
}

/// Start a `CASE WHEN …` expression. Add branches with
/// `.when(cond, then)` in order, then `.default(value)` for the
/// optional `ELSE` clause.
#[must_use]
pub fn case() -> CaseBuilder {
    CaseBuilder {
        branches: Vec::new(),
        default: None,
    }
}

/// Short for `Expr::Literal(v.into())`. Use it where a bare literal
/// could otherwise read as a column name:
///
/// ```ignore
/// case()
///     .when(Post::status.eq("draft"), value("Draft"))
///     .default(value("Published"))
/// ```
///
/// A bare literal works too (`case().when(..., "Draft")`); `value()`
/// only makes the intent clear.
#[must_use]
pub fn value(v: impl Into<super::SqlValue>) -> Expr {
    Expr::Literal(v.into())
}

impl CaseBuilder {
    /// Add a `WHEN <condition> THEN <then>` branch. `condition` is any
    /// [`WhereExpr`], usually from `Column::eq()`, `.and()` or
    /// `.or()`. `then` is any [`Expr`]: a literal, `F()`, a function
    /// call or a nested `Case`.
    #[must_use]
    pub fn when(mut self, condition: impl Into<WhereExpr>, then: impl Into<Expr>) -> Self {
        self.branches.push(CaseBranch {
            condition: condition.into(),
            then: then.into(),
        });
        self
    }

    /// Set the optional `ELSE` branch. SQL allows only one, so the
    /// last call wins.
    #[must_use]
    pub fn default(mut self, value: impl Into<Expr>) -> Self {
        self.default = Some(Box::new(value.into()));
        self
    }

    /// Finalize. Same as `Into<Expr>`, but a method helps when type
    /// inference needs it.
    #[must_use]
    pub fn build(self) -> Expr {
        Expr::Case {
            branches: self.branches,
            default: self.default,
        }
    }
}

impl From<CaseBuilder> for Expr {
    fn from(b: CaseBuilder) -> Self {
        b.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Filter, Op, SqlValue};

    fn predicate(col: &'static str, value: i64) -> WhereExpr {
        WhereExpr::Predicate(Filter {
            column: col,
            op: Op::Eq,
            value: SqlValue::I64(value),
        })
    }

    #[test]
    fn empty_builder_yields_case_with_no_branches() {
        let e: Expr = case().build();
        let Expr::Case { branches, default } = e else {
            panic!("expected Case variant")
        };
        assert!(branches.is_empty());
        assert!(default.is_none());
    }

    #[test]
    fn single_when_no_default_produces_one_branch() {
        let e: Expr = case().when(predicate("status", 1), 100_i64).build();
        let Expr::Case { branches, default } = e else {
            panic!()
        };
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].condition, predicate("status", 1));
        assert_eq!(branches[0].then, Expr::Literal(SqlValue::I64(100)));
        assert!(default.is_none());
    }

    #[test]
    fn multiple_branches_preserve_source_order() {
        let e: Expr = case()
            .when(predicate("a", 1), 10_i64)
            .when(predicate("a", 2), 20_i64)
            .when(predicate("a", 3), 30_i64)
            .build();
        let Expr::Case { branches, .. } = e else {
            panic!()
        };
        assert_eq!(branches.len(), 3);
        assert_eq!(branches[0].then, Expr::Literal(SqlValue::I64(10)));
        assert_eq!(branches[1].then, Expr::Literal(SqlValue::I64(20)));
        assert_eq!(branches[2].then, Expr::Literal(SqlValue::I64(30)));
    }

    #[test]
    fn default_is_stored_as_boxed_else() {
        let e: Expr = case()
            .when(predicate("status", 1), 100_i64)
            .default(999_i64)
            .build();
        let Expr::Case { default, .. } = e else {
            panic!()
        };
        assert_eq!(default.as_deref(), Some(&Expr::Literal(SqlValue::I64(999))));
    }

    #[test]
    fn last_default_call_wins() {
        let e: Expr = case().default(1_i64).default(2_i64).build();
        let Expr::Case { default, .. } = e else {
            panic!()
        };
        assert_eq!(default.as_deref(), Some(&Expr::Literal(SqlValue::I64(2))));
    }

    #[test]
    fn case_implements_into_expr() {
        // Compile-only check that `.into()` works in an
        // `impl Into<Expr>` slot.
        let _: Expr = case().when(predicate("x", 1), 1_i64).into();
    }

    #[test]
    fn value_sugars_a_literal_into_expr() {
        let e: Expr = value("hello");
        assert_eq!(e, Expr::Literal(SqlValue::String("hello".into())));

        let e: Expr = value(42_i64);
        assert_eq!(e, Expr::Literal(SqlValue::I64(42)));
    }
}
