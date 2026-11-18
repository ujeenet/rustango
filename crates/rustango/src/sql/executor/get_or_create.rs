//! `get_or_create` / `update_or_create` helpers.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 2. Both
//! helpers go through `FetcherPool::fetch` then either return
//! the existing row or invoke the caller's `create_fn` / `update_fn`
//! closure.

use super::{
    atomic, ExecError, FetcherPool, FetcherTx, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated,
    MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated, PoolTx,
};
use crate::core::Model;
use crate::sql::Pool;

/// v0.45 — `get_or_create`. Runs the queryset; if it
/// matches exactly one row return `(row, false)`; if it matches none
/// invoke `create_fn` to materialize a new instance + insert it and
/// return `(created, true)`. Matching multiple rows is a
/// programming error and returns
/// [`ExecError::MultipleRowsReturned`].
///
/// Not atomic: another writer can insert between the SELECT and the
/// INSERT. When `create_fn` fails on a UNIQUE or primary-key violation,
/// the query runs again and the row it now finds is returned, so a
/// UNIQUE constraint over the filtered columns makes concurrent callers safe.
/// Inside an [`atomic`](super::atomic) block on `pool`, the reads join the
/// block and `create_fn` runs in a savepoint: its writes roll back when it
/// fails and the block stays usable. Do not hold the block's guard across it.
/// In a PG repeatable-read or serializable block a lost race returns the
/// unique violation: no read sees the winner's row there.
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
///         p.insert(&pool).await?;
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
    match fetch_at_most_one(qs.clone(), "get_or_create", pool).await? {
        Some(row) => Ok((row, false)),
        None => match create(create_fn, pool).await {
            Ok(row) => Ok((row, true)),
            Err(e) => Ok((lost_race(qs, e, "get_or_create", pool, true).await?, false)),
        },
    }
}

/// [`get_or_create`] inside an open transaction: the read and `create_fn`
/// both run on `tx`. `create_fn` runs in a savepoint: when it loses an
/// insert race (a unique violation) it is rolled back and the winner's row
/// read instead, so `tx` stays usable. In a PG repeatable-read or
/// serializable transaction the unique violation is returned instead.
///
/// ```ignore
/// let (post, created) = rustango::sql::get_or_create_tx(
///     Post::objects().filter("slug", "hello"),
///     |tx| Box::pin(async move {
///         rustango::sql::insert_tx(tx, &query).await?;
///         Ok(post)
///     }),
///     &mut tx,
/// ).await?;
/// ```
///
/// # Errors
/// As [`get_or_create`].
pub async fn get_or_create_tx<'t, T, F>(
    qs: crate::query::QuerySet<T>,
    create_fn: F,
    tx: &mut PoolTx<'t>,
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
    F: for<'a> FnOnce(
        &'a mut PoolTx<'t>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'a>,
    >,
{
    match at_most_one(qs.clone().fetch_tx(tx).await?, "get_or_create")? {
        Some(row) => Ok((row, false)),
        None => create_or_winner_tx(qs, create_fn, tx, "get_or_create", true).await,
    }
}

/// [`update_or_create`] inside an open transaction: the read, `update_fn`
/// and `create_fn` all run on `tx`. A lost insert race updates the winner's
/// row, as in [`get_or_create_tx`].
///
/// # Errors
/// As [`update_or_create`].
pub async fn update_or_create_on<'t, T, UF, CF>(
    qs: crate::query::QuerySet<T>,
    update_fn: UF,
    create_fn: CF,
    tx: &mut PoolTx<'t>,
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
    UF: for<'a> FnOnce(
        &'a mut PoolTx<'t>,
        T,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'a>,
    >,
    CF: for<'a> FnOnce(
        &'a mut PoolTx<'t>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'a>,
    >,
{
    let existing = match at_most_one(qs.clone().fetch_tx(tx).await?, "update_or_create")? {
        Some(row) => row,
        None => match create_or_winner_tx(qs, create_fn, tx, "update_or_create", false).await? {
            (row, true) => return Ok((row, true)),
            (row, false) => row,
        },
    };
    Ok((update_fn(tx, existing).await?, false))
}

/// `create_fn` in a savepoint of `tx`; on a unique violation, the row a
/// concurrent writer inserted first, with `(row, created)`.
async fn create_or_winner_tx<'t, T, F>(
    qs: crate::query::QuerySet<T>,
    create_fn: F,
    tx: &mut PoolTx<'t>,
    op: &'static str,
    share: bool,
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
    F: for<'a> FnOnce(
        &'a mut PoolTx<'t>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'a>,
    >,
{
    let point = tx.dialect().quote_ident("rustango_create");
    tx.execute_unprepared(&format!("SAVEPOINT {point}")).await?;
    let err = match create_fn(tx).await {
        Ok(row) => {
            tx.execute_unprepared(&format!("RELEASE SAVEPOINT {point}"))
                .await?;
            return Ok((row, true));
        }
        Err(e) => e,
    };
    tx.execute_unprepared(&format!("ROLLBACK TO SAVEPOINT {point}"))
        .await?;
    tx.execute_unprepared(&format!("RELEASE SAVEPOINT {point}"))
        .await?;
    if !err.is_unique_violation() {
        return Err(err);
    }
    let qs = winner_read(qs, tx.dialect(), share);
    let row = at_most_one(qs.fetch_tx(tx).await?, op)?.ok_or(err)?;
    Ok((row, false))
}

/// The re-read after a lost race: MySQL's repeatable-read snapshot predates
/// the winner's row, so it reads with a lock (`share`, else for update).
fn winner_read<T: Model>(
    qs: crate::query::QuerySet<T>,
    dialect: &dyn super::Dialect,
    share: bool,
) -> crate::query::QuerySet<T> {
    if dialect.name() == "mysql" {
        qs.lock_rows_unless_set(share)
    } else {
        qs
    }
}

/// Run `create_fn`, in a savepoint of the `atomic` block on `pool` if
/// any, so a unique violation leaves the block usable for the retry.
async fn create<T, F, Fut>(create_fn: F, pool: &Pool) -> Result<T, ExecError>
where
    F: FnOnce(Pool) -> Fut,
    Fut: std::future::Future<Output = Result<T, ExecError>>,
{
    atomic::caught(pool, create_fn(pool.clone())).await
}

/// The one row `qs` matches, if any.
async fn fetch_at_most_one<T>(
    qs: crate::query::QuerySet<T>,
    op: &'static str,
    pool: &Pool,
) -> Result<Option<T>, ExecError>
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
    at_most_one(qs.fetch(pool).await?, op)
}

/// The only row of `rows`, if any; more than one is an error.
fn at_most_one<T: Model>(mut rows: Vec<T>, op: &'static str) -> Result<Option<T>, ExecError> {
    match rows.len() {
        0 => Ok(None),
        1 => Ok(Some(rows.remove(0))),
        n => Err(ExecError::MultipleRowsReturned {
            op,
            table: T::SCHEMA.table,
            count: n,
        }),
    }
}

/// After `create_fn` failed with `err`: the row a concurrent writer
/// inserted first when `err` is a unique violation, else `err`.
async fn lost_race<T>(
    qs: crate::query::QuerySet<T>,
    err: ExecError,
    op: &'static str,
    pool: &Pool,
    share: bool,
) -> Result<T, ExecError>
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
    if !err.is_unique_violation() {
        return Err(err);
    }
    // Only a block's re-read joins a snapshot. `update_or_create` takes the
    // update lock at once, so two losers cannot deadlock upgrading.
    let qs = if atomic::in_block(pool) {
        winner_read(qs, pool.dialect(), share)
    } else {
        qs
    };
    fetch_at_most_one(qs, op, pool).await?.ok_or(err)
}

/// v0.45 — `update_or_create`. Runs the queryset; if
/// it matches exactly one row, invoke `update_fn` to mutate it +
/// save the changes and return `(updated, false)`; if it matches
/// none, invoke `create_fn` and return `(created, true)`. Matching
/// multiple rows returns [`ExecError::MultipleRowsReturned`].
///
/// Not atomic, as [`get_or_create`]: a create that loses to a concurrent
/// insert on a UNIQUE constraint updates the winner's row instead.
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
///         existing.save(&pool).await?;
///         Ok(existing)
///     },
///     |pool| async move {
///         let mut p = Post { /* defaults */ };
///         p.insert(&pool).await?;
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
    let existing = match fetch_at_most_one(qs.clone(), "update_or_create", pool).await? {
        Some(row) => row,
        None => match create(create_fn, pool).await {
            Ok(row) => return Ok((row, true)),
            Err(e) => lost_race(qs, e, "update_or_create", pool, false).await?,
        },
    };
    Ok((update_fn(pool.clone(), existing).await?, false))
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
