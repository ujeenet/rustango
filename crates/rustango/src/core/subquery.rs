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

use super::expr::{CaseBranch, Expr};
use super::query::{
    AggregateExpr, AggregateQuery, CtFilter, Op, RelAggKind, RelCorrelation, SelectQuery, WhereExpr,
};
use super::schema::{GenericReverseRelation, M2MRelation, ReverseRelation};
use super::SqlValue;

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

/// Build the correlated `EXISTS (SELECT 1 FROM <child_table> WHERE
/// <child_fk_column> = <outer>.<self_pk_column>)` predicate for the
/// given [`ReverseRelation`] — issue #830 sub-piece backing
/// [`crate::query::QuerySet::where_has`].
///
/// The inner `SelectQuery` projects nothing (the dialect writer
/// reads no rows out — `EXISTS` only cares about row presence) and
/// joins via `OuterRef(self_pk_column)`. The writer's scope stack
/// rewrites `OuterRef` to the parent queryset's table qualifier at
/// emit time, so the SQL stays unambiguous across dialects.
#[must_use]
pub fn reverse_has_exists(rel: &ReverseRelation) -> WhereExpr {
    let inner = SelectQuery {
        where_clause: WhereExpr::ExprCompare {
            lhs: Expr::Column(rel.child_fk_column),
            op: Op::Eq,
            rhs: Expr::OuterRef(rel.self_pk_column),
        },
        ..SelectQuery::new(rel.child_schema)
    };
    WhereExpr::Exists(Box::new(inner))
}

/// `NOT EXISTS (subquery)` counterpart of [`reverse_has_exists`].
/// Backs [`crate::query::QuerySet::where_doesnt_have`].
#[must_use]
pub fn reverse_has_not_exists(rel: &ReverseRelation) -> WhereExpr {
    let inner = SelectQuery {
        where_clause: WhereExpr::ExprCompare {
            lhs: Expr::Column(rel.child_fk_column),
            op: Op::Eq,
            rhs: Expr::OuterRef(rel.self_pk_column),
        },
        ..SelectQuery::new(rel.child_schema)
    };
    WhereExpr::NotExists(Box::new(inner))
}

/// Build the correlated `(SELECT <agg> FROM <child_table> WHERE
/// <child_fk_column> = <outer>.<self_pk_column>)` scalar-aggregate
/// subquery for the given [`ReverseRelation`] — issue #830. Backs both
/// the count-comparator [`crate::query::QuerySet::where_has_count`]
/// (slice 3) and the eager relation aggregates
/// [`crate::query::QuerySet::annotate_count`] / `annotate_sum` / … (slice
/// 4/5).
///
/// The returned [`Expr`] is an [`Expr::AggregateSubquery`]. The inner
/// [`AggregateQuery`] projects `agg` over the **child** table (no
/// `GROUP BY`, so it yields exactly one scalar row) and correlates to
/// the parent via `OuterRef(self_pk_column)`; the writer's scope stack
/// rewrites that `OuterRef` to the enclosing query's table qualifier at
/// emit time, identically across PG / MySQL / SQLite.
#[must_use]
pub fn reverse_has_aggregate(rel: &ReverseRelation, agg: AggregateExpr) -> Expr {
    let inner = AggregateQuery {
        model: rel.child_schema,
        where_clause: WhereExpr::ExprCompare {
            lhs: Expr::Column(rel.child_fk_column),
            op: Op::Eq,
            rhs: Expr::OuterRef(rel.self_pk_column),
        },
        group_by: Vec::new(),
        // The alias is inert in a scalar subquery (the outer context
        // reads the single value, not the name) but the aggregate writer
        // requires one.
        aggregates: vec![("c".into(), agg)],
        aliases: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
    };
    Expr::AggregateSubquery(Box::new(inner))
}

/// Correlated `(SELECT COUNT(*) FROM <child> WHERE <child_fk> =
/// <outer>.<pk>)` — the count specialization of
/// [`reverse_has_aggregate`], suitable as the left-hand side of a
/// count-comparison predicate (`… > 3`). Backs
/// [`crate::query::QuerySet::where_has_count`].
#[must_use]
pub fn reverse_has_count(rel: &ReverseRelation) -> Expr {
    reverse_has_aggregate(rel, AggregateExpr::Count(None))
}

/// Wrap any existence predicate (`EXISTS` / `NOT EXISTS` / `RelExists`,
/// from the reverse-FK, M2M, or GFK builders) in `CASE WHEN <exists>
/// THEN 1 ELSE 0 END` so it can be **projected** as a column — backs
/// [`crate::query::QuerySet::annotate_exists`] (`withExists`) for every
/// relation kind.
///
/// Integer `1`/`0` literals (rather than booleans) are deliberate: the
/// projected `<rel>_exists` column then decodes as `SqlValue::I64(0|1)`
/// identically on PG / MySQL / SQLite. A bare `EXISTS(…)` projection
/// would return a native `bool` on Postgres but `0|1` on the other two,
/// so the dict-row value would vary by backend.
#[must_use]
pub fn exists_as_int(exists: WhereExpr) -> Expr {
    Expr::Case {
        branches: vec![CaseBranch {
            condition: exists,
            then: Expr::Literal(SqlValue::I64(1)),
        }],
        default: Some(Box::new(Expr::Literal(SqlValue::I64(0)))),
    }
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

// ----------------------------------------------------------- M2M (#830)

/// `[NOT ]EXISTS (SELECT 1 FROM <through> WHERE <src_col> =
/// <outer>.<self_pk>)` — many-to-many relation existence over the
/// junction table. `self_pk` is the parent model's primary-key column.
/// Backs [`crate::query::QuerySet::where_has`] for M2M relations.
#[must_use]
pub fn m2m_has_exists(m2m: &M2MRelation, self_pk: &'static str, negated: bool) -> WhereExpr {
    WhereExpr::RelExists {
        table: m2m.through,
        correlation: RelCorrelation::Fk {
            fk_column: m2m.src_col,
            outer_column: self_pk,
            ct: None,
        },
        negated,
    }
}

/// Correlated many-to-many aggregate. `Count` counts **junction rows**
/// (`SELECT COUNT(*) FROM <through> WHERE <src_col> = <outer>.<self_pk>`);
/// `Sum`/`Avg`/`Max`/`Min` aggregate `column` on the **target** table,
/// reached through the junction:
///
/// ```text
/// (SELECT <agg>(<column>) FROM <to>
///  WHERE <to>.id IN (SELECT <dst_col> FROM <through>
///                    WHERE <src_col> = <outer>.<self_pk>))
/// ```
///
/// The target PK is assumed to be `"id"` — rustango's surrogate-PK
/// convention; [`M2MRelation`] doesn't carry the target PK column.
#[must_use]
pub fn m2m_has_aggregate(
    m2m: &M2MRelation,
    self_pk: &'static str,
    kind: RelAggKind,
    column: Option<&'static str>,
) -> Expr {
    match kind {
        RelAggKind::Count => Expr::RelAggregate {
            kind: RelAggKind::Count,
            column: None,
            table: m2m.through,
            correlation: RelCorrelation::Fk {
                fk_column: m2m.src_col,
                outer_column: self_pk,
                ct: None,
            },
        },
        _ => Expr::RelAggregate {
            kind,
            column,
            table: m2m.to,
            correlation: RelCorrelation::Membership {
                target_pk: "id",
                through: m2m.through,
                dst_col: m2m.dst_col,
                src_col: m2m.src_col,
                outer_column: self_pk,
            },
        },
    }
}

// ----------------------------------------------------- GFK / generic (#830)

/// Build the content-type discriminator for a generic-FK relation. The
/// registry coordinates (`rustango_content_types` / `id` / `table`)
/// mirror [`crate::contenttypes::ContentType`]; they're inlined here
/// rather than read off `ContentType::SCHEMA` so `core` stays free of a
/// dependency on the higher-level contenttypes app module.
fn ct_filter(rel: &GenericReverseRelation, parent_table: &'static str) -> CtFilter {
    CtFilter {
        ct_column: rel.ct_column,
        parent_table,
        ct_table: "rustango_content_types",
        ct_pk: "id",
        ct_table_col: "table",
    }
}

/// `[NOT ]EXISTS (SELECT 1 FROM <child> WHERE <pk_column> =
/// <outer>.<self_pk> AND <ct_column> = (SELECT id FROM
/// rustango_content_types WHERE "table" = '<parent_table>'))` — generic
/// (polymorphic) relation existence. `parent_table` is the querying
/// model's table name (a compile-time constant), used to resolve its
/// content-type id via the nested subquery — no async lookup needed.
#[must_use]
pub fn generic_has_exists(
    rel: &GenericReverseRelation,
    parent_table: &'static str,
    negated: bool,
) -> WhereExpr {
    WhereExpr::RelExists {
        table: rel.child_schema.table,
        correlation: RelCorrelation::Fk {
            fk_column: rel.pk_column,
            outer_column: rel.self_pk_column,
            ct: Some(ct_filter(rel, parent_table)),
        },
        negated,
    }
}

/// Correlated generic-FK aggregate over a `child` column. The child
/// table carries the data column, so a single-table aggregate suffices
/// (no junction). `Count` ignores `column`.
#[must_use]
pub fn generic_has_aggregate(
    rel: &GenericReverseRelation,
    parent_table: &'static str,
    kind: RelAggKind,
    column: Option<&'static str>,
) -> Expr {
    Expr::RelAggregate {
        kind,
        column,
        table: rel.child_schema.table,
        correlation: RelCorrelation::Fk {
            fk_column: rel.pk_column,
            outer_column: rel.self_pk_column,
            ct: Some(ct_filter(rel, parent_table)),
        },
    }
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
