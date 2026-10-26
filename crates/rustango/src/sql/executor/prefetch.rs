//! Tri-dialect `prefetch_related` helpers — `fetch_with_prefetch_pool`
//! and `fetch_with_prefetch_filtered` (issue #298 / T2.1).
//!
//! Extracted from `executor/mod.rs` as part of #116 step 6. The PG-only
//! `_on` variants (`annotate_count_children`, `annotate_count_children_on`,
//! `fetch_with_prefetch`) plus the `HasPkValue` trait + `extract_pk_value`
//! helper stay in `mod.rs` — they're tightly coupled to PG's `PgRow` /
//! `PgPool` types and the schema-mode tenancy connection path that lives
//! in mod.rs proper.

use super::{
    extract_pk_value, ExecError, FetcherPool, FkPkAccess, HasPkValue, LoadRelated, MaybeMyFromRow,
    MaybeMyLoadRelated, MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
};
use crate::core::Model;
use crate::sql::Pool;

/// `fetch_with_prefetch` against either backend. Bi-dialect counterpart
/// of `fetch_with_prefetch` — fetches parents + children in two
/// round trips and stitches each child onto its parent via
/// [`FkPkAccess`]. Bounds add [`MaybeMyFromRow`] + [`MaybeMyLoadRelated`]
/// over the `&PgPool` version's bounds; every `#[derive(Model)]`
/// type satisfies them automatically.
///
/// `FkPkAccess` doesn't need a MySQL counterpart — it reads i64
/// values from struct fields after decoding, with no Row dependency,
/// so the same trait works against either backend.
///
/// # Errors
/// As the PG `fetch_with_prefetch` family.
pub async fn fetch_with_prefetch_pool<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + FkPkAccess
        + Send
        + Unpin,
{
    fetch_with_prefetch_filtered(
        parent_qs,
        child_fk_column,
        crate::query::QuerySet::<C>::new(),
        pool,
    )
    .await
}

/// `fetch_with_prefetch_pool` with a user-supplied **filtered** child
/// queryset. Issue #298 / T2.1.
///
/// Drop-in replacement for [`fetch_with_prefetch_pool`] when the
/// caller wants to **filter / order / limit** the child fetch:
///
/// ```ignore
/// use rustango::core::Column as _;
/// use rustango::query::QuerySet;
///
/// // Authors + their PUBLISHED posts, newest first.
/// let published_posts = QuerySet::<Post>::default()
///     .filter("published", true)
///     .order_by(&[("created", true)]);
///
/// let authors_with_posts: Vec<(Author, Vec<Post>)> =
///     fetch_with_prefetch_filtered(
///         Author::objects(),
///         "author",
///         published_posts,
///         &pool,
///     ).await?;
/// ```
///
/// # Caveat — global vs. per-parent `LIMIT`
///
/// `child_qs.limit(N)` applies **globally** across the joined fetch,
/// not per parent. A per-parent slice (LATERAL JOIN on PG,
/// `ROW_NUMBER() OVER (PARTITION BY ...)` elsewhere) is a follow-up.
///
/// # Errors
/// As [`fetch_with_prefetch_pool`]; [`ExecError::InListUnsplittable`] when the
/// parents exceed one `IN` list and `child_qs` has a limit or offset.
pub async fn fetch_with_prefetch_filtered<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    child_qs: crate::query::QuerySet<C>,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + FkPkAccess
        + Send
        + Unpin,
{
    let parents: Vec<P> = parent_qs.fetch(pool).await?;
    let on = target_column::<P, C>(child_fk_column);
    // The IN list AND-composes with the caller's filters, order and limit.
    // Over the bind cap the keys go in batches; a limit or offset then refuses.
    stitch(parents, on, |keys| async {
        let children = reverse_children(pool, &child_qs, child_fk_column, keys).await?;
        Ok(children
            .into_iter()
            .filter_map(|c| Some((c.__rustango_fk_pk_value(child_fk_column)?, c)))
            .collect())
    })
    .await
}

/// `child_qs` narrowed to rows whose `child_fk_column` is one of `keys`.
async fn reverse_children<C>(
    pool: &Pool,
    child_qs: &crate::query::QuerySet<C>,
    child_fk_column: &'static str,
    keys: Vec<crate::core::SqlValue>,
) -> Result<Vec<C>, ExecError>
where
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    super::fetch_in_chunks(pool, child_qs, keys, |keys| {
        child_qs
            .clone()
            .filter_op(
                child_fk_column,
                crate::core::Op::In,
                crate::core::SqlValue::List(keys),
            )
            .fetch(pool)
    })
    .await
}

/// Prefetch the relation `relation` of every parent: a many-to-many or a
/// `reverse_has` reverse FK, by name, as Django's `prefetch_related("tags")`.
///
/// ```ignore
/// let posts: Vec<(Post, Vec<Tag>)> =
///     fetch_with_prefetch_related(Post::objects(), "tags", &pool).await?;
/// ```
///
/// One query for the parents and one per relation, its `IN` list chunked
/// under the backend's bind cap. A reverse FK keys on the relation's
/// `self_pk_column`; a many-to-many joins its through table, so a target
/// shared by several parents is a copy under each.
///
/// # Errors
/// As [`fetch_with_prefetch_filtered`];
/// [`QueryError::UnknownField`](crate::core::QueryError::UnknownField) for
/// an unknown name and [`ExecError::PrefetchTargetMismatch`] when the
/// relation does not point at `C`.
pub async fn fetch_with_prefetch_related<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    relation: &str,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
{
    let child_qs = crate::query::QuerySet::<C>::new();
    fetch_with_prefetch_related_filtered(parent_qs, relation, child_qs, pool).await
}

/// [`fetch_with_prefetch_related`] with a filtered, ordered child queryset.
/// A limit on `child_qs` is global, not per parent.
///
/// # Errors
/// As [`fetch_with_prefetch_related`].
pub async fn fetch_with_prefetch_related_filtered<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    relation: &str,
    child_qs: crate::query::QuerySet<C>,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
{
    // Resolved first, so a bad name costs no query.
    let named = Named::resolve::<P, C>(relation)?;
    let parents: Vec<P> = parent_qs.fetch(pool).await?;
    match named {
        Named::Reverse { field, column, on } => {
            stitch(parents, on, |keys| async {
                let children = reverse_children(pool, &child_qs, field, keys).await?;
                Ok(children
                    .into_iter()
                    .filter_map(|c| Some((c.__rustango_column_value(column)?, c)))
                    .collect())
            })
            .await
        }
        Named::M2m(rel) => {
            stitch(parents, None, |keys| {
                m2m_children(pool, rel, &child_qs, keys)
            })
            .await
        }
    }
}

/// A relation [`fetch_with_prefetch_related`] loads, resolved by name.
enum Named {
    /// Children whose `field` (SQL `column`) holds the parent's `on` value.
    Reverse {
        field: &'static str,
        column: &'static str,
        on: Option<&'static str>,
    },
    /// Targets linked through a junction keyed by the parent's pk.
    M2m(&'static crate::core::M2MRelation),
}

impl Named {
    /// Reverse FK first, then M2M: the order `where_has` resolves names in.
    fn resolve<P: Model, C: Model>(relation: &str) -> Result<Self, ExecError> {
        let mismatch = |expected: &'static str| ExecError::PrefetchTargetMismatch {
            model: P::SCHEMA.name,
            relation: relation.to_owned(),
            expected,
            got: C::SCHEMA.table,
        };
        if let Some(rel) = P::reverse_relations().iter().find(|r| r.name == relation) {
            if rel.child_schema.table != C::SCHEMA.table {
                return Err(mismatch(rel.child_schema.table));
            }
            let field = C::SCHEMA
                .field(rel.child_fk_column)
                .or_else(|| C::SCHEMA.field_by_column(rel.child_fk_column))
                .ok_or_else(|| {
                    ExecError::Query(crate::core::QueryError::UnknownField {
                        model: C::SCHEMA.name,
                        field: rel.child_fk_column.to_owned(),
                    })
                })?;
            let on =
                (Some(rel.self_pk_column) != P::SCHEMA.pk_column()).then_some(rel.self_pk_column);
            return Ok(Self::Reverse {
                field: field.name,
                column: field.column,
                on,
            });
        }
        if let Some(rel) = P::SCHEMA.m2m.iter().find(|m| m.name == relation) {
            if rel.to != C::SCHEMA.table {
                return Err(mismatch(rel.to));
            }
            return Ok(Self::M2m(rel));
        }
        Err(ExecError::Query(crate::core::QueryError::UnknownField {
            model: P::SCHEMA.name,
            field: relation.to_owned(),
        }))
    }
}

/// The junction's alias in the many-to-many prefetch join.
const M2M_ALIAS: &str = "__rustango_m2m";

/// `child_qs` inner-joined to `rel`'s junction on rows whose source is one
/// of `keys`, each target paired with that source: one row per link.
async fn m2m_children<C>(
    pool: &Pool,
    rel: &'static crate::core::M2MRelation,
    child_qs: &crate::query::QuerySet<C>,
    keys: Vec<crate::core::SqlValue>,
) -> Result<Vec<(crate::core::SqlValue, C)>, ExecError>
where
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    use crate::core::joins::aliased;
    use crate::core::{Filter, Join, JoinKind, Op, SqlValue, WhereExpr};
    let pk = C::SCHEMA.pk_column().ok_or(ExecError::MissingPrimaryKey {
        table: C::SCHEMA.table,
    })?;
    let junction = crate::sql::m2m::junction(rel.through, &[rel.src_col, rel.dst_col]);
    let src_ty = junction.field_by_column(rel.src_col).map(|f| f.ty);
    let base = child_qs.clone().compile()?;
    let linked = |keys: Option<Vec<SqlValue>>| {
        let mut on = vec![WhereExpr::ExprCompare {
            lhs: aliased(M2M_ALIAS, rel.dst_col),
            op: Op::Eq,
            rhs: aliased(C::SCHEMA.table, pk),
        }];
        if let Some(keys) = keys {
            on.push(WhereExpr::Predicate(Filter {
                column: rel.src_col,
                op: Op::In,
                value: SqlValue::List(keys),
            }));
        }
        let mut q = base.clone();
        q.joins.push(Join {
            target: junction,
            alias: M2M_ALIAS,
            kind: JoinKind::Inner,
            on: WhereExpr::And(on),
            project: vec![rel.src_col],
        });
        q
    };
    let label = crate::core::joins::joined_label(M2M_ALIAS, rel.src_col);
    let key = (label.as_str(), src_ty);
    super::fetch_select_in_chunks(pool, &linked(None), child_qs.is_sliced(), keys, |keys| {
        let q = linked(Some(keys));
        async move { super::select_rows_keyed_on(pool, &q, key).await }
    })
    .await
}

/// Pair each parent with the children `fetch` returns keyed by its `on`
/// value (else its pk). A parent the query returned `n` times costs `n`
/// child fetches: `C` is not `Clone`.
async fn stitch<P, C, F, Fut>(
    parents: Vec<P>,
    on: Option<&'static str>,
    mut fetch: F,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + HasPkValue,
    F: FnMut(Vec<crate::core::SqlValue>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<(crate::core::SqlValue, C)>, ExecError>>,
{
    use std::collections::HashMap;
    if parents.is_empty() {
        return Ok(Vec::new());
    }
    P::SCHEMA
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: P::SCHEMA.table,
        })?;
    // Each distinct key in first-seen order, with how many parents carry it.
    let mut keys: Vec<(String, crate::core::SqlValue, usize)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for parent in &parents {
        let pk = parent_key(parent, on)?;
        if matches!(pk, crate::core::SqlValue::Null) {
            continue;
        }
        let text = pk.to_display_string();
        match at.get(&text) {
            Some(&i) => keys[i].2 += 1,
            None => {
                at.insert(text.clone(), keys.len());
                keys.push((text, pk, 1));
            }
        }
    }
    let copies = keys.iter().map(|k| k.2).max().unwrap_or(0);
    let mut rounds: Vec<HashMap<String, Vec<C>>> = Vec::with_capacity(copies);
    for round in 0..copies {
        let batch = keys
            .iter()
            .filter(|k| k.2 > round)
            .map(|k| k.1.clone())
            .collect();
        let mut grouped: HashMap<String, Vec<C>> = HashMap::new();
        for (fk, child) in fetch(batch).await? {
            grouped
                .entry(fk.to_display_string())
                .or_default()
                .push(child);
        }
        rounds.push(grouped);
    }
    let mut taken: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let pk = parent_key(&parent, on)?;
        if matches!(pk, crate::core::SqlValue::Null) {
            out.push((parent, Vec::new()));
            continue;
        }
        let text = pk.to_display_string();
        let copy = taken.entry(text.clone()).or_insert(0);
        let kids = rounds[*copy].remove(&text).unwrap_or_default();
        *copy += 1;
        out.push((parent, kids));
    }
    Ok(out)
}

/// The value `C.child_fk_column` stores for `parent`, for the reverse
/// `<parent>::<child>_set` helpers: its `on` column, else its pk.
///
/// # Errors
/// [`ExecError::PrefetchColumnUnreadable`] when `P` cannot read that column.
#[doc(hidden)]
pub fn __reverse_fk_key<P: Model + HasPkValue, C: Model>(
    parent: &P,
    child_fk_column: &str,
) -> Result<crate::core::SqlValue, ExecError> {
    parent_key(parent, target_column::<P, C>(child_fk_column))
}

/// The parent column `C.child_fk_column` points at, when not `P`'s pk.
fn target_column<P: Model, C: Model>(child_fk_column: &str) -> Option<&'static str> {
    use crate::core::Relation;
    let field = C::SCHEMA
        .field(child_fk_column)
        .or_else(|| C::SCHEMA.field_by_column(child_fk_column))?;
    let on = match field.relation? {
        Relation::Fk { on, .. } | Relation::O2O { on, .. } => on,
    };
    (Some(on) != P::SCHEMA.pk_column()).then_some(on)
}

/// The value children store for `parent`: its `on` column, else its pk.
fn parent_key<P: Model + HasPkValue>(
    parent: &P,
    on: Option<&'static str>,
) -> Result<crate::core::SqlValue, ExecError> {
    match on {
        None => Ok(extract_pk_value(parent)),
        Some(column) => {
            parent
                .__rustango_column_value(column)
                .ok_or(ExecError::PrefetchColumnUnreadable {
                    table: P::SCHEMA.table,
                    column,
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parent_key, ExecError, HasPkValue};
    use crate::core::{Model, ModelSchema, SqlValue};

    /// A parent with a hand-written `HasPkValue`, which reads only its pk.
    struct HandWritten;

    impl Model for HandWritten {
        const SCHEMA: &'static ModelSchema = <crate::contenttypes::ContentType as Model>::SCHEMA;
    }

    impl HasPkValue for HandWritten {
        fn __rustango_pk_value_impl(&self) -> SqlValue {
            SqlValue::I64(1)
        }
    }

    #[test]
    fn an_unreadable_fk_target_column_is_an_error_not_the_pk() {
        let err = parent_key(&HandWritten, Some("code")).expect_err("not the pk");
        assert!(
            matches!(
                err,
                ExecError::PrefetchColumnUnreadable { column: "code", .. }
            ),
            "{err}"
        );
        assert!(err.to_string().contains(".code`"), "{err}");
        assert_eq!(parent_key(&HandWritten, None).unwrap(), SqlValue::I64(1));
    }
}
