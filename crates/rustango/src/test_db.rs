//! Test-time DB isolation — Django's `TestCase` transaction
//! wrapping. Issue #39 partial.
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

use crate::sql::{transaction_pool, ExecError, Pool, PoolTx};

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

#[cfg(test)]
mod tests {
    // Live-database tests need a real connection pool. The
    // pure-logic tests cover the macro shape via type-check; the
    // rollback behavior is exercised by integration tests in
    // crates that wire a real Pool.

    use super::with_rollback;

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
            }
        };
    }
}
