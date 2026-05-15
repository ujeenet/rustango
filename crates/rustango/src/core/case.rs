//! `CASE WHEN … THEN … ELSE … END` conditional expressions (issue #4).
//!
//! The fourth slice of the ORM Expression DSL epic. Builds on the
//! [`crate::core::Expr`] tree from #1 and the function dispatcher
//! from #2/#3.
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
//! ## Conditions accept any [`WhereExpr`]
//!
//! The `WHEN` predicate uses the same shape as a `where_()` clause —
//! `Column::eq()`, `Column::gt()`, `.and()`, `.or()`, `Not(...)` all
//! work. Each branch's condition is a full tree, so arbitrarily
//! complex predicates are expressible.
//!
//! ## `ELSE` is optional
//!
//! Omitting `.default(...)` produces `CASE WHEN … END` (no `ELSE`).
//! On every dialect, a `CASE` that matches no branch with no `ELSE`
//! returns `NULL`. Add `.default(...)` to make the fallback explicit.
//!
//! ## Tri-dialect: identical SQL
//!
//! `CASE WHEN … THEN … ELSE … END` is SQL-92 standard syntax and
//! works identically on PG, MySQL, and SQLite. The writer emits the
//! same string for every backend.
//!
//! [`WhereExpr`]: crate::core::WhereExpr

use super::expr::{CaseBranch, Expr};
use super::query::WhereExpr;

/// Builder for an [`Expr::Case`]. Constructed via [`case()`]; finalize
/// by passing into anything that takes `impl Into<Expr>` (e.g.
/// `set_expr`, `annotate`, `Column::eq_expr`). The builder always
/// produces a well-formed `Case` even when no branches are added —
/// the writer rejects empty-branches case expressions at emit time
/// so users see an error before the database does.
#[must_use]
pub struct CaseBuilder {
    branches: Vec<CaseBranch>,
    default: Option<Box<Expr>>,
}

/// Start a `CASE WHEN …` expression. Chain `.when(cond, then)` for
/// each branch in order, then optionally `.default(value)` to set
/// the `ELSE` clause. Finalize implicitly when passed to anything
/// expecting `impl Into<Expr>`.
#[must_use]
pub fn case() -> CaseBuilder {
    CaseBuilder {
        branches: Vec::new(),
        default: None,
    }
}

/// Sugar for `Expr::Literal(v.into())`. Mirrors Django's `Value()`
/// — useful at call sites where a bare literal could be mistaken for
/// a column reference and you want to be explicit:
///
/// ```ignore
/// case()
///     .when(Post::status.eq("draft"), value("Draft"))
///     .default(value("Published"))
/// ```
///
/// The bare-literal form (`case().when(..., "Draft")`) also works
/// via `From<&str> for Expr`; `value()` is for emphasis.
#[must_use]
pub fn value(v: impl Into<super::SqlValue>) -> Expr {
    Expr::Literal(v.into())
}

impl CaseBuilder {
    /// Append a `WHEN <condition> THEN <then>` branch. `condition` is
    /// anything convertible to [`WhereExpr`] — most commonly a
    /// `TypedFilter` from `Column::eq()` / `.and()` / `.or()`. `then`
    /// is any [`Expr`] (literal, `F()`, function call, nested `Case`).
    #[must_use]
    pub fn when(mut self, condition: impl Into<WhereExpr>, then: impl Into<Expr>) -> Self {
        self.branches.push(CaseBranch {
            condition: condition.into(),
            then: then.into(),
        });
        self
    }

    /// Set the optional `ELSE` branch. Last call wins if invoked
    /// repeatedly — only one `ELSE` is legal in SQL.
    #[must_use]
    pub fn default(mut self, value: impl Into<Expr>) -> Self {
        self.default = Some(Box::new(value.into()));
        self
    }

    /// Finalize. Same as `Into<Expr>::into(builder)` — provided as a
    /// method when type inference would otherwise need help.
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
        // Compile-only check that `case().build()` is interchangeable
        // with explicit `.into()` in any `impl Into<Expr>` slot.
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
