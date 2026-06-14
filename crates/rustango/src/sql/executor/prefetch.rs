//! Tri-dialect `prefetch_related` helpers — `fetch_with_prefetch_pool`
//! and `fetch_with_prefetch_filtered` (Django's `Prefetch(queryset=...)`
//! shape, issue #298 / T2.1).
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
    let parents: Vec<P> = parent_qs.fetch(pool).await?;
    if parents.is_empty() {
        return Ok(Vec::new());
    }

    // Same `SqlValue`-keyed grouping as `fetch_with_prefetch` so
    // non-i64 FK PKs work end-to-end on both Postgres and MySQL.
    let pk_field = P::SCHEMA
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: P::SCHEMA.table,
        })?;
    let mut parent_pks: Vec<crate::core::SqlValue> = Vec::with_capacity(parents.len());
    for parent in &parents {
        let pk = extract_pk_value(parent);
        if !matches!(pk, crate::core::SqlValue::Null) {
            parent_pks.push(pk);
        }
    }
    {
        let mut seen = std::collections::HashSet::new();
        parent_pks.retain(|v| seen.insert(v.to_display_string()));
    }
    if parent_pks.is_empty() {
        return Ok(parents.into_iter().map(|p| (p, Vec::new())).collect());
    }

    let children: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter_op(
            child_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(parent_pks),
        )
        .fetch(pool)
        .await?;

    let mut grouped: std::collections::HashMap<String, Vec<C>> = std::collections::HashMap::new();
    for child in children {
        let Some(fk_pk) = child.__rustango_fk_pk_value(child_fk_column) else {
            continue;
        };
        grouped
            .entry(fk_pk.to_display_string())
            .or_default()
            .push(child);
    }

    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let pk = extract_pk_value(&parent).to_display_string();
        let kids = grouped.remove(&pk).unwrap_or_default();
        out.push((parent, kids));
    }
    let _ = pk_field;
    Ok(out)
}

/// `fetch_with_prefetch_pool` with a user-supplied **filtered** child
/// queryset — Django's `Prefetch(queryset=...)` shape. Issue #298 /
/// T2.1.
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
/// not per parent. Django's per-parent slice on prefetch (`.filter(
/// post_set__lt=...)` semantics + LATERAL JOIN on PG, `ROW_NUMBER()
/// OVER (PARTITION BY ...)` elsewhere) is a follow-up.
///
/// # Errors
/// As [`fetch_with_prefetch_pool`].
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
    if parents.is_empty() {
        return Ok(Vec::new());
    }

    let pk_field = P::SCHEMA
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: P::SCHEMA.table,
        })?;
    let mut parent_pks: Vec<crate::core::SqlValue> = Vec::with_capacity(parents.len());
    for parent in &parents {
        let pk = extract_pk_value(parent);
        if !matches!(pk, crate::core::SqlValue::Null) {
            parent_pks.push(pk);
        }
    }
    {
        let mut seen = std::collections::HashSet::new();
        parent_pks.retain(|v| seen.insert(v.to_display_string()));
    }
    if parent_pks.is_empty() {
        return Ok(parents.into_iter().map(|p| (p, Vec::new())).collect());
    }

    // Inject the IN-predicate by `filter_op` on top of whatever the
    // user already chained — preserves their order_by / limit /
    // existing filters. The IN-predicate AND-composes with the user's
    // WHERE clause (QuerySet default-ANDs raw filters).
    let children: Vec<C> = child_qs
        .filter_op(
            child_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(parent_pks),
        )
        .fetch(pool)
        .await?;

    let mut grouped: std::collections::HashMap<String, Vec<C>> = std::collections::HashMap::new();
    for child in children {
        let Some(fk_pk) = child.__rustango_fk_pk_value(child_fk_column) else {
            continue;
        };
        grouped
            .entry(fk_pk.to_display_string())
            .or_default()
            .push(child);
    }

    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let pk = extract_pk_value(&parent).to_display_string();
        let kids = grouped.remove(&pk).unwrap_or_default();
        out.push((parent, kids));
    }
    let _ = pk_field;
    Ok(out)
}
