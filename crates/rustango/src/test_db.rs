//! Database isolation for tests, in four tiers.
//!
//! There are no test base classes here. Each tier is a helper you
//! wrap around the test body, so pick the cheapest one that works:
//!
//! | Tier                   | Helper                                 | Use when …                                              |
//! |------------------------|----------------------------------------|---------------------------------------------------------|
//! | no database            | plain `#[tokio::test]`                 | no DB access — fastest.                                 |
//! | rolled-back writes     | [`with_rollback`]                      | reads / writes that should be rolled back at end.       |
//! | committed writes       | [`with_truncate_after`]                | code under test commits (signals, on_commit hooks).     |
//! | real socket            | [`crate::test_server::LiveServer`]     | needs a real listening socket (browser, websockets).    |
//!
//! The two DB helpers differ in what happens between tests:
//!
//! - `with_rollback` runs the body in a transaction that always
//!   rolls back. It is the fastest, but nothing that waits for a
//!   real commit runs: no signals, no `on_commit` hooks, no commit
//!   triggers.
//! - `with_truncate_after` lets the body commit, then clears the
//!   listed tables. The body sees committed state and the next test
//!   starts clean, at the cost of the truncate.
//!
//! The common case:
//!
//! ```ignore
//! use rustango::test_db::with_rollback;
//!
//! #[tokio::test]
//! async fn create_and_count() {
//!     let pool = test_pool().await;
//!     with_rollback(&pool, |tx| Box::pin(async move {
//!         // Rolled back when the closure returns.
//!         let mut tx = tx.lock().await?;
//!         insert_tx(&mut tx, &article_q("First")).await?;
//!         insert_tx(&mut tx, &article_q("Second")).await?;
//!         Ok(())
//!     })).await.unwrap();
//!
//!     // The two articles are gone — rollback happened on return.
//!     assert_eq!(Article::objects().count(&pool).await.unwrap(), 0);
//! }
//! ```
//!
//! ## Why not `atomic()`?
//!
//! [`crate::sql::atomic`] commits on `Ok`. A test wants the rollback
//! every time, so [`with_rollback`] rolls back instead and still
//! hands back the closure's value. It is an `atomic` block otherwise:
//! the closure gets the same [`AtomicTx`], and an `atomic()` on the same
//! pool inside it runs in a savepoint that rolls back with it.
//!
//! ## Limits
//!
//! - The rollback covers one closure. Tests share the pool, so it
//!   only undoes what this test wrote.
//! - Parallel tests can still race on rows they did not insert. Take
//!   a suite-wide `tokio::Mutex` when a test touches global state.
//! - `on_commit` callbacks never fire under `with_rollback`, by
//!   design, and any the closure registered are cleared.
//! - Nested calls and nested `atomic()` blocks on the same pool act as
//!   savepoints. The outer rollback throws away their work too.
//! - Drop the `tx.lock()` guard before a nested `atomic()` or a
//!   `bulk_insert_pool`: while it is held they fail with `NestedAtomic`.

use std::future::Future;
use std::pin::Pin;

use crate::sql::{raw_execute_tx, write_transaction_pool, AtomicTx, ExecError, Pool};

/// Run `f` in a transaction that ALWAYS rolls back when the closure
/// returns, on `Ok` and on `Err` alike. The rollback happens after
/// the closure's value is captured, so you get that value back.
///
/// The closure gets an [`AtomicTx`]: lock it per statement. An `atomic()`
/// on `pool` inside it is a savepoint of this transaction (#1761).
///
/// An `Err` from the closure passes straight through. An `Ok` turns
/// into an error only if the rollback itself fails.
///
/// # Errors
/// - The closure's own error.
/// - `BEGIN` or `ROLLBACK` driver errors.
pub async fn with_rollback<F, T>(pool: &Pool, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> Pin<Box<dyn Future<Output = Result<T, ExecError>> + Send + 'tx>>,
{
    crate::sql::rolled_back(pool, f).await
}

/// [`with_rollback`] with the `Box::pin(async move { … })` wrapper
/// written for you. Behaves the same:
///
/// ```ignore
/// rustango::with_rollback!(&pool, |tx| {
///     insert_tx(&mut *tx.lock().await?, &q).await?;
///     // ... assertions ...
///     Ok(())
/// }).await
/// ```
#[macro_export]
macro_rules! with_rollback {
    ($pool:expr, |$tx:ident| $body:block) => {{
        $crate::test_db::with_rollback($pool, move |$tx| Box::pin(async move { $body }))
    }};
}

/// Run `f`, then clear `tables` whatever the result, even when `f`
/// panics (the panic is re-raised after the clear). This is the
/// `TransactionTestCase` shape: the closure's writes commit, so
/// signals, `on_commit` hooks and commit triggers all fire, and the
/// teardown leaves the tables clean for the next test.
///
/// The clear is [`truncate_tables`]: one transaction, any table order.
///
/// The closure's value comes back unchanged. A failed truncate
/// becomes the error only when the closure succeeded, so a noisy
/// teardown cannot hide the real failure.
///
/// **Concurrency**: this commits real rows, so tests using the same
/// tables must run one at a time behind a suite-wide
/// `tokio::sync::Mutex<()>`. Otherwise one worker's truncate wipes
/// another's setup.
///
/// List only the tables the test touches. Clearing every model's
/// table would tie each test to every other app. For a full wipe,
/// use `manage flush --yes`.
///
/// ```ignore
/// use rustango::test_db::with_truncate_after;
///
/// #[tokio::test]
/// async fn create_article_fires_post_save_signal() {
///     let pool = test_pool().await;
///     let _g = SUITE_MUTEX.lock().await;
///     with_truncate_after(&pool, &["articles"], || async move {
///         create_article("First").await?; // COMMITS — post_save fires
///         assert_signal_fired();
///         Ok(())
///     }).await.unwrap();
///     // The article is gone — truncate happened on return.
/// }
/// ```
///
/// # Errors
/// - The closure's own error (transitively).
/// - Truncate driver errors (only when the closure succeeded).
pub async fn with_truncate_after<F, Fut, T>(
    pool: &Pool,
    tables: &[&str],
    f: F,
) -> Result<T, ExecError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ExecError>> + Send,
{
    // Catch a panic in the body so the clear still runs (#1959).
    let mut body = Box::pin(async move { f().await });
    let result = std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    })
    .await;
    let truncate = truncate_tables(pool, tables).await;
    match (result, truncate) {
        (Err(panic), _) => std::panic::resume_unwind(panic),
        (Ok(Ok(v)), Ok(())) => Ok(v),
        (Ok(Ok(_)), Err(e)) => Err(e),
        (Ok(Err(e)), _) => Err(e),
    }
}

/// Clear every table in `tables`, in one transaction, in any order.
/// Public so fixtures and manual teardown can reuse it outside
/// [`with_truncate_after`]. The statements come from
/// [`crate::sql::Dialect::clear_tables_sql`]:
/// - **Postgres**: `TRUNCATE … RESTART IDENTITY CASCADE`; ids restart
///   and referencing tables are cleared too.
/// - **MySQL**: `DELETE` with FK checks off for that statement; ids
///   keep counting and rows in unlisted tables that point here stay.
/// - **SQLite**: `DELETE` with FK checks deferred to `COMMIT`, which
///   fails if an unlisted table still points at a cleared row.
///
/// An empty slice does nothing.
///
/// # Errors
/// The first driver error; the whole clear is then rolled back.
pub async fn truncate_tables(pool: &Pool, tables: &[&str]) -> Result<(), ExecError> {
    if tables.is_empty() {
        return Ok(());
    }
    let mut tx = write_transaction_pool(pool).await?;
    for sql in pool.dialect().clear_tables_sql(tables) {
        raw_execute_tx(&mut tx, &sql, Vec::new()).await?;
    }
    tx.commit().await.map_err(ExecError::Driver)?;
    Ok(())
}

/// [`with_truncate_after`] in the same macro shape as
/// [`with_rollback!`](crate::with_rollback):
///
/// ```ignore
/// rustango::with_truncate_after!(&pool, &["articles", "comments"], {
///     create_article("First").await?;
///     create_comment("…").await?;
///     Ok(())
/// }).await
/// ```
#[macro_export]
macro_rules! with_truncate_after {
    ($pool:expr, $tables:expr, $body:block) => {{
        $crate::test_db::with_truncate_after($pool, $tables, move || async move { $body })
    }};
}

#[cfg(test)]
mod tests {
    // These only type-check the macro shape. Rollback behaviour
    // needs a real pool, so integration tests cover it.

    use super::{truncate_tables, with_rollback, with_truncate_after};

    #[test]
    fn macro_and_function_compile() {
        // Compile-only: pins the function and macro signatures so a
        // refactor cannot change them quietly.
        let _ = || async {
            // The closure never runs. `unimplemented!()` diverges, so
            // every use of `pool` is unreachable and the binding
            // reads as unused; hence both allows.
            #[allow(unreachable_code, unused_variables, clippy::diverging_sub_expression)]
            {
                let pool: &crate::sql::Pool = unimplemented!();
                let _r: Result<i32, _> =
                    with_rollback(pool, |_tx| Box::pin(async move { Ok(42) })).await;
                let _r2: Result<i32, _> = crate::with_rollback!(pool, |tx| {
                    let _: &crate::sql::AtomicTx = tx;
                    Ok(42)
                })
                .await;
                let _r3: Result<i32, _> =
                    with_truncate_after(pool, &["t1", "t2"], || async move { Ok(7) }).await;
                let _r4: Result<i32, _> =
                    crate::with_truncate_after!(pool, &["t1"], { Ok(7) }).await;
                let _r5: Result<(), _> = truncate_tables(pool, &["t1"]).await;
            }
        };
    }
}
