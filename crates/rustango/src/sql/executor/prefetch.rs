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
    let parents: Vec<P> = parent_qs.fetch(pool).await?;
    let base = crate::query::QuerySet::<C>::new();
    stitch(parents, child_fk_column, |keys| {
        super::fetch_in_chunks(pool, &base, keys, |keys| {
            crate::query::QuerySet::<C>::new()
                .filter_op(
                    child_fk_column,
                    crate::core::Op::In,
                    crate::core::SqlValue::List(keys),
                )
                .fetch(pool)
        })
    })
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
    // The IN list AND-composes with the caller's filters, order and limit.
    // Over the bind cap the keys go in batches; a limit or offset then refuses.
    stitch(parents, child_fk_column, |keys| {
        super::fetch_in_chunks(pool, &child_qs, keys, |keys| {
            child_qs
                .clone()
                .filter_op(
                    child_fk_column,
                    crate::core::Op::In,
                    crate::core::SqlValue::List(keys),
                )
                .fetch(pool)
        })
    })
    .await
}

/// Pair each parent with the children `fetch` returns for its key. A parent
/// the query returned `n` times costs `n` child fetches: `C` is not `Clone`.
async fn stitch<P, C, F, Fut>(
    parents: Vec<P>,
    child_fk_column: &'static str,
    mut fetch: F,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + HasPkValue,
    C: FkPkAccess,
    F: FnMut(Vec<crate::core::SqlValue>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<C>, ExecError>>,
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
        let pk = extract_pk_value(parent);
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
        for child in fetch(batch).await? {
            if let Some(fk) = child.__rustango_fk_pk_value(child_fk_column) {
                grouped
                    .entry(fk.to_display_string())
                    .or_default()
                    .push(child);
            }
        }
        rounds.push(grouped);
    }
    let mut taken: HashMap<String, usize> = HashMap::new();
    Ok(parents
        .into_iter()
        .map(|parent| {
            let pk = extract_pk_value(&parent);
            if matches!(pk, crate::core::SqlValue::Null) {
                return (parent, Vec::new());
            }
            let text = pk.to_display_string();
            let copy = taken.entry(text.clone()).or_insert(0);
            let kids = rounds[*copy].remove(&text).unwrap_or_default();
            *copy += 1;
            (parent, kids)
        })
        .collect())
}
