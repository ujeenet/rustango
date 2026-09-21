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
//!         // Inserts here are visible to assertions inside the
//!         // closure, but rolled back when it returns.
//!         insert_tx(tx, &article_q("First")).await?;
//!         insert_tx(tx, &article_q("Second")).await?;
//!
//!         let count = count_tx::<Article>(tx).await?;
//!         assert_eq!(count, 2);
//!         Ok(())
//!     })).await.unwrap();
//!
//!     // The two articles are gone — rollback happened on return.
//! }
//! ```
//!
//! ## Why not `atomic()`?
//!
//! [`crate::sql::atomic`] commits on `Ok`. A test wants the rollback
//! every time, so [`with_rollback`] rolls back instead and still
//! hands back the closure's value.
//!
//! ## Limits
//!
//! - The rollback covers one closure. Tests share the pool, so it
//!   only undoes what this test wrote.
//! - Parallel tests can still race on rows they did not insert. Take
//!   a suite-wide `tokio::Mutex` when a test touches global state.
//! - `on_commit` callbacks never fire under `with_rollback`, by
//!   design, and any the closure registered are cleared.
//! - Nested calls act as savepoints. An outer rollback throws away
//!   inner work even if the inner call committed.

use std::future::Future;
use std::pin::Pin;

use crate::sql::{raw_execute_pool, transaction_pool, ExecError, Pool, PoolTx};

/// Run `f` in a transaction that ALWAYS rolls back when the closure
/// returns, on `Ok` and on `Err` alike. The rollback happens after
/// the closure's value is captured, so you get that value back.
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
        &'tx mut PoolTx<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<T, ExecError>> + Send + 'tx>>,
{
    let mut tx = transaction_pool(pool).await?;
    let result = f(&mut tx).await;
    // Always roll back. A failed rollback becomes the error only
    // when the closure succeeded; otherwise its error wins.
    let rollback = tx.rollback().await;
    match (result, rollback) {
        (Ok(v), Ok(())) => Ok(v),
        (Ok(_), Err(e)) => Err(ExecError::Driver(e)),
        (Err(e), _) => Err(e),
    }
}

/// [`with_rollback`] with the `Box::pin(async move { … })` wrapper
/// written for you. Behaves the same:
///
/// ```ignore
/// rustango::with_rollback!(&pool, |tx| {
///     insert_tx(tx, &q).await?;
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

/// Run `f`, then clear `tables` whatever the result. This is the
/// `TransactionTestCase` shape: the closure's writes commit, so
/// signals, `on_commit` hooks and commit triggers all fire, and the
/// teardown leaves the tables clean for the next test.
///
/// Like `manage flush`, the method depends on the dialect:
/// - **Postgres**: one `TRUNCATE TABLE … RESTART IDENTITY CASCADE`.
/// - **MySQL / SQLite**: `DELETE FROM "<table>"` per table, in the
///   given order. Sequences are not reset.
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
    let result = f().await;
    let truncate = truncate_tables(pool, tables).await;
    match (result, truncate) {
        (Ok(v), Ok(())) => Ok(v),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), _) => Err(e),
    }
}

/// Clear every table in `tables`. Public so fixtures and manual
/// teardown can reuse it outside [`with_truncate_after`].
///
/// Postgres gets one `TRUNCATE`; MySQL and SQLite get a
/// `DELETE FROM` per table. The per-table path does not stop at the
/// first failure, so the rest are still cleared.
///
/// An empty slice does nothing.
///
/// # Errors
/// - The first driver error. Later ones are dropped.
pub async fn truncate_tables(pool: &Pool, tables: &[&str]) -> Result<(), ExecError> {
    if tables.is_empty() {
        return Ok(());
    }
    let dialect = pool.dialect().name();
    if dialect == "postgres" {
        let quoted: Vec<String> = tables
            .iter()
            .map(|t| format!(r#""{}""#, t.replace('"', r#""""#)))
            .collect();
        let sql = format!(
            "TRUNCATE TABLE {} RESTART IDENTITY CASCADE",
            quoted.join(", "),
        );
        raw_execute_pool(pool, &sql, Vec::new()).await?;
        Ok(())
    } else {
        let mut first_err: Option<ExecError> = None;
        for table in tables {
            let sql = format!(r#"DELETE FROM "{}""#, table.replace('"', r#""""#));
            if let Err(e) = raw_execute_pool(pool, &sql, Vec::new()).await {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
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
                    let _: &mut crate::sql::PoolTx<'_> = tx;
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
