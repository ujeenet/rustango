//! Django-shape `get_or_create` / `update_or_create` helpers.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 2. Both
//! helpers go through `FetcherPool::fetch` then either return
//! the existing row or invoke the caller's `create_fn` / `update_fn`
//! closure.

use super::{
    ExecError, FetcherPool, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow,
    MaybeSqliteFromRow, MaybeSqliteLoadRelated,
};
use crate::core::Model;
use crate::sql::Pool;

/// v0.45 — Django-style `get_or_create`. Runs the queryset; if it
/// matches exactly one row return `(row, false)`; if it matches none
/// invoke `create_fn` to materialize a new instance + insert it and
/// return `(created, true)`. Matching multiple rows is a
/// programming error and returns
/// [`ExecError::MultipleRowsReturned`].
///
/// Like Django's helper, this is **not atomic** without an enclosing
/// transaction — between the SELECT and the INSERT another writer
/// could insert a colliding row. For race-free behaviour pair it
/// with `Pool::begin()` or with a UNIQUE constraint that surfaces
/// the conflict via the existing `upsert()` machinery.
///
/// The closure receives an **owned** `Pool` (cheap to clone — it's
/// an `Arc` internally). That sidesteps Rust's async-closure
/// lifetime inference: borrowed `&Pool` inside a future returned
/// from a closure trips the compiler's `'1 must outlive '2` rule.
///
/// # Example
///
/// ```ignore
/// let (post, created) = rustango::sql::get_or_create(
///     Post::objects().filter("slug", "hello"),
///     |pool| async move {
///         let mut p = Post {
///             id: Auto::Unset,
///             slug: "hello".into(),
///             title: "Hello".into(),
///         };
///         p.insert_pool(&pool).await?;
///         Ok(p)
///     },
///     &pool,
/// ).await?;
/// ```
///
/// # Errors
/// - [`ExecError::MultipleRowsReturned`] when the filter matches >1 row.
/// - Whatever the `create_fn` closure returns when matching no rows.
/// - Whatever [`FetcherPool::fetch`] returns.
pub async fn get_or_create<T, F, Fut>(
    qs: crate::query::QuerySet<T>,
    create_fn: F,
    pool: &Pool,
) -> Result<(T, bool), ExecError>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
    F: FnOnce(Pool) -> Fut,
    Fut: std::future::Future<Output = Result<T, ExecError>>,
{
    let mut rows = qs.fetch(pool).await?;
    match rows.len() {
        0 => Ok((create_fn(pool.clone()).await?, true)),
        1 => Ok((rows.remove(0), false)),
        n => Err(ExecError::MultipleRowsReturned {
            op: "get_or_create",
            table: T::SCHEMA.table,
            count: n,
        }),
    }
}

/// v0.45 — Django-style `update_or_create`. Runs the queryset; if
/// it matches exactly one row, invoke `update_fn` to mutate it +
/// save the changes and return `(updated, false)`; if it matches
/// none, invoke `create_fn` and return `(created, true)`. Matching
/// multiple rows returns [`ExecError::MultipleRowsReturned`].
///
/// Same atomicity caveat as [`get_or_create`] — wrap in a
/// transaction or rely on a UNIQUE constraint for race-free
/// semantics.
///
/// Both closures receive an **owned** `Pool` for the same
/// async-lifetime reason as [`get_or_create`].
///
/// # Example
///
/// ```ignore
/// let (post, created) = rustango::sql::update_or_create(
///     Post::objects().filter("slug", "hello"),
///     |pool, mut existing| async move {
///         existing.title = "New title".into();
///         existing.save_pool(&pool).await?;
///         Ok(existing)
///     },
///     |pool| async move {
///         let mut p = Post { /* defaults */ };
///         p.insert_pool(&pool).await?;
///         Ok(p)
///     },
///     &pool,
/// ).await?;
/// ```
///
/// # Errors
/// As [`get_or_create`].
pub async fn update_or_create<T, UF, UFut, CF, CFut>(
    qs: crate::query::QuerySet<T>,
    update_fn: UF,
    create_fn: CF,
    pool: &Pool,
) -> Result<(T, bool), ExecError>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
    UF: FnOnce(Pool, T) -> UFut,
    UFut: std::future::Future<Output = Result<T, ExecError>>,
    CF: FnOnce(Pool) -> CFut,
    CFut: std::future::Future<Output = Result<T, ExecError>>,
{
    let mut rows = qs.fetch(pool).await?;
    match rows.len() {
        0 => Ok((create_fn(pool.clone()).await?, true)),
        1 => {
            let existing = rows.remove(0);
            let updated = update_fn(pool.clone(), existing).await?;
            Ok((updated, false))
        }
        n => Err(ExecError::MultipleRowsReturned {
            op: "update_or_create",
            table: T::SCHEMA.table,
            count: n,
        }),
    }
}

impl<T> crate::query::QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Chainable form of [`get_or_create`]. Sugar for
    /// `get_or_create(self, create_fn, pool)` so call sites read as
    /// `Post::objects().filter(...).get_or_create(|pool| ..., &pool)`.
    ///
    /// Returns `(row, created)` where `created` is `true` when the
    /// closure was invoked.
    ///
    /// # Errors
    /// As [`get_or_create`].
    pub async fn get_or_create<F, Fut>(
        self,
        create_fn: F,
        pool: &Pool,
    ) -> Result<(T, bool), ExecError>
    where
        F: FnOnce(Pool) -> Fut,
        Fut: std::future::Future<Output = Result<T, ExecError>>,
    {
        get_or_create(self, create_fn, pool).await
    }

    /// Eloquent `Model::firstOrCreate($attributes, $values)` alias
    /// for [`Self::get_or_create`]. Identical semantics; the
    /// separate name keeps Laravel muscle-memory call sites
    /// readable.
    ///
    /// # Errors
    /// As [`Self::get_or_create`].
    pub async fn first_or_create<F, Fut>(
        self,
        create_fn: F,
        pool: &Pool,
    ) -> Result<(T, bool), ExecError>
    where
        F: FnOnce(Pool) -> Fut,
        Fut: std::future::Future<Output = Result<T, ExecError>>,
    {
        self.get_or_create(create_fn, pool).await
    }

    /// Chainable form of [`update_or_create`]. Sugar for
    /// `update_or_create(self, update_fn, create_fn, pool)`.
    ///
    /// Returns `(row, created)` where `created` is `true` when the
    /// `create_fn` closure was invoked (i.e. no row matched the
    /// queryset).
    ///
    /// # Errors
    /// As [`update_or_create`].
    pub async fn update_or_create<UF, UFut, CF, CFut>(
        self,
        update_fn: UF,
        create_fn: CF,
        pool: &Pool,
    ) -> Result<(T, bool), ExecError>
    where
        UF: FnOnce(Pool, T) -> UFut,
        UFut: std::future::Future<Output = Result<T, ExecError>>,
        CF: FnOnce(Pool) -> CFut,
        CFut: std::future::Future<Output = Result<T, ExecError>>,
    {
        update_or_create(self, update_fn, create_fn, pool).await
    }
}
