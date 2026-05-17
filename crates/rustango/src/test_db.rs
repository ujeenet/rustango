//! Test-time DB isolation — Django's `TestCase` / `TransactionTestCase`
//! analogs. Issue #39.
//!
//! ## Four-tier shape
//!
//! Django ships four test base classes with distinct DB semantics.
//! rustango is a function-style framework, so the analogs are
//! helpers that callers wrap around the test body:
//!
//! | Django class            | rustango analog                        | Use when …                                              |
//! |-------------------------|----------------------------------------|---------------------------------------------------------|
//! | `SimpleTestCase`        | plain `#[tokio::test]`                 | no DB access — fastest.                                 |
//! | `TestCase`              | [`with_rollback`]                      | reads / writes that should be rolled back at end.       |
//! | `TransactionTestCase`   | [`with_truncate_after`]                | code under test commits (signals, on_commit hooks).     |
//! | `LiveServerTestCase`    | [`crate::test_server::LiveServer`]     | needs a real listening socket (Selenium, websockets).   |
//!
//! ## Why a separate helper per tier?
//!
//! Each tier has a different "what happens between tests" guarantee:
//!
//! - `with_rollback` wraps the body in a transaction that ALWAYS
//!   rolls back. Fastest. But invisible to code paths that check for
//!   a committed state (signals, `on_commit` hooks, FK triggers
//!   firing on real commit).
//! - `with_truncate_after` commits real rows during the body, then
//!   truncates the listed tables after — so the body sees committed
//!   state, and the next test starts clean. Slower than
//!   `with_rollback` because every test pays the truncate cost.
//!
//! Django's `TestCase` wraps each test method in a transaction that
//! always rolls back, so mutations during one test never leak into
//! the next. Rust has no test classes; the analog is an explicit
//! helper that test code wraps around its body:
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
//! ## Why a separate helper instead of `atomic()`?
//!
//! [`crate::sql::atomic`] commits on `Ok` and rolls back on `Err`.
//! Tests want the rollback unconditionally so the schema stays
//! clean for the next test. [`with_rollback`] swaps the commit
//! branch for a rollback while preserving the closure's return
//! value.
//!
//! ## Caveats vs Django
//!
//! - **Per-test isolation only**: the rollback wraps a single
//!   closure. Tests still share a process-wide DB connection pool;
//!   the rollback only resets data this test inserted.
//! - **Concurrency**: parallel tests still race on shared rows
//!   they didn't insert. Pair with a suite-wide `tokio::Mutex` if
//!   the test touches process-global state (per the project's
//!   global-state mutex convention).
//! - **`on_commit` callbacks**: they NEVER fire here, by design —
//!   the tx always rolls back, so deferred work would be a phantom.
//!   `with_rollback` also clears any callbacks the closure registered.
//! - **SAVEPOINTs**: nested calls behave as nested savepoints via
//!   sqlx's transaction shape. Outer rollback discards inner work
//!   even if inner committed.
//!
//! Issue #39 partial — full TestCase / TransactionTestCase /
//! SimpleTestCase / LiveServerTestCase hierarchy is a separate slice.

use std::future::Future;
use std::pin::Pin;

use crate::sql::{raw_execute_pool, transaction_pool, ExecError, Pool, PoolTx};

/// Run `f` inside a transaction that ALWAYS rolls back when the
/// closure returns, regardless of whether the closure returned
/// `Ok` or `Err`. The transaction-rollback step happens after the
/// closure's value is captured, so callers see the closure's
/// original result.
///
/// On `Err` the closure result is returned as-is. On `Ok` the
/// closure result is returned after the rollback completes
/// successfully; if the rollback itself fails (network blip /
/// connection drop) the closure's Ok is converted to that
/// driver error.
///
/// # Errors
/// - The closure's own error (transitively).
/// - `BEGIN` / `ROLLBACK` driver errors.
pub async fn with_rollback<F, T>(pool: &Pool, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx mut PoolTx<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<T, ExecError>> + Send + 'tx>>,
{
    let mut tx = transaction_pool(pool).await?;
    let result = f(&mut tx).await;
    // ALWAYS roll back, regardless of `result`. A failing rollback
    // (driver/network) is reported as the new error only when the
    // closure succeeded — otherwise the closure's own error takes
    // priority.
    let rollback = tx.rollback().await;
    match (result, rollback) {
        (Ok(v), Ok(())) => Ok(v),
        (Ok(_), Err(e)) => Err(ExecError::Driver(e)),
        (Err(e), _) => Err(e),
    }
}

/// Sugar around [`with_rollback`] that wraps the body in
/// `Box::pin(async move { … })` so callers don't have to. Identical
/// semantics:
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

/// Run `f` to completion, then truncate `tables` regardless of the
/// closure's result. Django's `TransactionTestCase` analog: the
/// closure's writes COMMIT (so it sees real post-commit state —
/// signals fire, `on_commit` hooks fire, FK triggers fire), and the
/// teardown step clears those tables so the next test starts clean.
///
/// Per-dialect strategy mirrors [`crate::migrate::manage`] `flush`:
/// - **Postgres**: one `TRUNCATE TABLE t1, t2, ... RESTART IDENTITY
///   CASCADE` statement. Atomic, FK-aware, sequence-resetting.
/// - **MySQL / SQLite**: per-table `DELETE FROM "<table>"` in
///   the listed order. Sequences are NOT reset.
///
/// The closure's return value is preserved unchanged. Truncate
/// failures surface as the new error ONLY when the closure
/// succeeded — otherwise the closure's error takes priority (so the
/// real failure isn't masked by a noisy teardown).
///
/// **Concurrency**: this helper commits real rows. Tests calling it
/// against the same tables must serialize (the project convention
/// is a suite-wide `tokio::sync::Mutex<()>`) — see the
/// "Tests on global state need a mutex" memory note. Truncate races
/// across parallel `cargo test` workers would otherwise wipe each
/// other's setup mid-flight.
///
/// **Passing tables explicitly**: callers list only the tables they
/// touch. Truncating every registered model table would couple every
/// test to every other app's tables — slow and brittle. If a test
/// really wants the "wipe everything" shape, `manage flush --yes`
/// does that.
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

/// Truncate every table in `tables`, dialect-aware. Public so callers
/// can reuse the per-dialect clear logic outside [`with_truncate_after`]
/// (custom test fixtures, manual teardown).
///
/// On Postgres this is one `TRUNCATE` statement; on MySQL/SQLite it's
/// per-table `DELETE FROM`. Returns the first driver error, but does
/// NOT short-circuit — every remaining table is still attempted on
/// the per-table path so partial cleanup happens even when one fails.
///
/// Passing an empty slice is a no-op (returns `Ok(())`).
///
/// # Errors
/// - The first driver error encountered (per-table path collects
///   subsequent errors but reports only the first).
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

/// Sugar around [`with_truncate_after`] mirroring the
/// [`with_rollback!`](crate::with_rollback) macro shape:
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
    // Live-database tests need a real connection pool. The
    // pure-logic tests cover the macro shape via type-check; the
    // rollback behavior is exercised by integration tests in
    // crates that wire a real Pool.

    use super::{truncate_tables, with_rollback, with_truncate_after};

    #[test]
    fn macro_and_function_compile() {
        // Compile-only: pin the function + macro signatures so a
        // refactor doesn't silently change them. The body never
        // executes — the closure isn't called.
        let _ = || async {
            // This closure never executes; the test exists to catch
            // macro hygiene regressions.
            #[allow(unreachable_code, clippy::diverging_sub_expression)]
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
