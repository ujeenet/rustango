//! Subquery, `EXISTS` and `OuterRef` builders.
//!
//! Primitives that turn a [`SelectQuery`] into something you can
//! embed inside a larger queryset.
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
//! [`outer_ref("col")`][outer_ref] returns an [`Expr::OuterRef`]. The
//! SQL writer resolves it against the query one level out:
//!
//! ```text
//! SELECT … FROM "author" WHERE EXISTS (
//!     SELECT … FROM "book" WHERE "book"."author_id" = "author"."id"
//!                                                       ^^^^^^^^
//!                                                       OuterRef
//! )
//! ```
//!
//! The writer keeps a stack of scopes. Every `EXISTS`, `NOT EXISTS`,
//! `IN (SELECT …)` and scalar [`subquery`] pushes a frame, and
//! `outer_ref("col")` reads the nearest one out. Deeper nesting works
//! the same way.
//!
//! ## Validation happens on the inner queryset
//!
//! These builders take an already-compiled [`SelectQuery`], so a bad
//! column name is reported by the inner `queryset.compile()` call, not
//! when the outer query runs. Build the subquery first, propagate `?`,
//! then embed it.

use super::expr::{CaseBranch, Expr};
use super::query::{
    AggregateExpr, AggregateQuery, CtFilter, Op, RelAggKind, RelCorrelation, SelectQuery, WhereExpr,
};
use super::schema::{GenericReverseRelation, M2MRelation, ReverseRelation};
use super::SqlValue;

/// `EXISTS (subquery)` — true when the subquery returns at least one
/// row.
#[must_use]
pub fn exists(subquery: SelectQuery) -> WhereExpr {
    WhereExpr::Exists(Box::new(subquery))
}

/// `NOT EXISTS (subquery)` — true when the subquery returns no rows.
/// Use it to find rows in A with no related row in B.
#[must_use]
pub fn not_exists(subquery: SelectQuery) -> WhereExpr {
    WhereExpr::NotExists(Box::new(subquery))
}

/// `<column> IN (subquery)`. Use it when the inner query needs joins
/// or aggregation that a flat [`crate::core::Op::In`] list cannot
/// express.
///
/// `column` is the outer column. `subquery` must select one column.
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

/// Scalar subquery — `(SELECT … FROM …)`. Usable as an `Expr`
/// wherever `set_expr`, `eq_expr` or a CASE THEN slot takes a value.
///
/// Shape the inner queryset yourself (`.limit(1)`, one projected
/// column) so it returns one column and one row. Anything else is a
/// database error at runtime.
#[must_use]
pub fn subquery(inner: SelectQuery) -> Expr {
    Expr::Subquery(Box::new(inner))
}

/// Project a correlated scalar subquery as an annotation column,
/// such as "the title of each author's newest book".
/// Wraps the subquery as an [`AggregateExpr`], so it fits
/// [`crate::query::QuerySet::annotate`] and
/// [`crate::query::AggregateBuilder::annotate`]. Any [`outer_ref`]
/// inside `inner` is correlated to the enclosing query.
///
/// Shape `inner` to one column and at most one row
/// (`.values_list_flat(col)` + `.limit(1)`). When nothing matches the
/// column is `NULL`; more than one row is a database error.
///
/// ```ignore
/// // Newest book title per author.
/// let newest = Book::objects()
///     .where_(Book::author_id.eq_expr(outer_ref("id")))
///     .order_by(&[("id", true)])
///     .limit(1)
///     .values_list_flat("title")
///     .compile()?;
/// let rows = Author::objects()
///     .annotate("newest", scalar_subquery(newest))
///     .fetch_values(&pool).await?;
/// ```
#[must_use]
pub fn scalar_subquery(inner: SelectQuery) -> AggregateExpr {
    AggregateExpr::RelatedAggregate(Box::new(Expr::Subquery(Box::new(inner))))
}

/// Correlated `EXISTS (SELECT 1 FROM <child_table> WHERE
/// <child_fk_column> = <outer>.<self_pk_column>)` for a
/// [`ReverseRelation`]. Backs [`crate::query::QuerySet::where_has`].
///
/// The inner query projects nothing, since `EXISTS` only asks whether
/// a row is there. It joins through `OuterRef(self_pk_column)`, which
/// the writer qualifies with the parent table.
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

/// Correlated `(SELECT <agg> FROM <child_table> WHERE
/// <child_fk_column> = <outer>.<self_pk_column>)` scalar-aggregate
/// subquery for a [`ReverseRelation`]. Backs
/// [`crate::query::QuerySet::where_has_count`] and the relation
/// aggregates [`crate::query::QuerySet::annotate_count`],
/// `annotate_sum` and friends.
///
/// The inner [`AggregateQuery`] runs `agg` over the child table with
/// no `GROUP BY`, so it yields one scalar row.
#[must_use]
pub fn reverse_has_aggregate(rel: &ReverseRelation, agg: AggregateExpr) -> Expr {
    let inner = AggregateQuery {
        model: rel.child_schema,
        joins: Vec::new(),
        where_clause: WhereExpr::ExprCompare {
            lhs: Expr::Column(rel.child_fk_column),
            op: Op::Eq,
            rhs: Expr::OuterRef(rel.self_pk_column),
        },
        group_by: Vec::new(),
        // The alias is unused in a scalar subquery, but the aggregate
        // writer requires one.
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
/// <outer>.<pk>)` — [`reverse_has_aggregate`] with `COUNT`. Use it as
/// the left side of a count comparison such as `… > 3`.
#[must_use]
pub fn reverse_has_count(rel: &ReverseRelation) -> Expr {
    reverse_has_aggregate(rel, AggregateExpr::Count(None))
}

/// Wrap any existence predicate (`EXISTS`, `NOT EXISTS`, `RelExists`)
/// in `CASE WHEN <exists> THEN 1 ELSE 0 END` so it can be projected as
/// a column. Backs [`crate::query::QuerySet::annotate_exists`].
///
/// The `1`/`0` integers are deliberate: the column then decodes as
/// `SqlValue::I64(0|1)` on every backend. A bare `EXISTS(…)`
/// projection would give a native `bool` on Postgres but `0|1` on the
/// other two.
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

/// `OuterRef("col")` — use a column of the enclosing query inside a
/// correlated subquery.
///
/// It only works inside a subquery wrapper ([`exists`],
/// [`not_exists`], [`in_subquery`], [`subquery`]); anywhere else the
/// writer returns
/// [`crate::sql::SqlError::OuterRefOutsideSubquery`]. `column` is a
/// column on the outer model and is emitted as
/// `"<outer_table>"."<col>"`, so it stays unambiguous when inner and
/// outer tables share a column name.
#[must_use]
pub fn outer_ref(column: &'static str) -> Expr {
    Expr::OuterRef(column)
}

// ----------------------------------------------------------------- M2M

/// `[NOT ]EXISTS (SELECT 1 FROM <through> WHERE <src_col> =
/// <outer>.<self_pk>)` — many-to-many existence, checked on the
/// junction table. `self_pk` is the parent model's primary-key
/// column. Backs [`crate::query::QuerySet::where_has`] for M2M.
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
/// The target PK is assumed to be `"id"`, rustango's surrogate-PK
/// convention, because [`M2MRelation`] does not carry it.
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

// ----------------------------------------------------------- GFK / generic

/// Content-type discriminator for a generic-FK relation. The registry
/// names (`rustango_content_types` / `id` / `table`) match
/// [`crate::contenttypes::ContentType`]. They are inlined instead of
/// read from `ContentType::SCHEMA` so `core` does not depend on the
/// contenttypes app module.
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
/// rustango_content_types WHERE "table" = '<parent_table>'))` —
/// generic (polymorphic) relation existence. `parent_table` is the
/// querying model's table name, a compile-time constant. The nested
/// subquery resolves its content-type id, so no async lookup is
/// needed.
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

/// Correlated generic-FK aggregate over a child column. The child
/// table holds the data, so no junction table is involved. `Count`
/// ignores `column`.
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

    // The SelectQuery-shaped builders need a real `Model`-derived
    // schema, so they are covered by `tests/subquery_expressions.rs`
    // and `tests/subquery_expressions_live.rs`. Only the builder that
    // needs no schema is tested here.

    #[test]
    fn outer_ref_stores_column_name() {
        let e = outer_ref("id");
        assert_eq!(e, Expr::OuterRef("id"));
    }
}
