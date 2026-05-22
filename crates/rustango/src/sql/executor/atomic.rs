//! `atomic()` + `on_commit()` — Django's `transaction.atomic` +
//! `transaction.on_commit`, rolled into one helper. Issue #44.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 4. The
//! `atomic!` declarative-macro sugar lives at the crate root via
//! `#[macro_export]` regardless of which module declares it; we keep
//! it co-located with [`atomic`] for readability.

use super::{transaction_pool, ExecError, PoolTx};
use crate::sql::Pool;

tokio::task_local! {
    /// Active callback queue for the current `atomic` scope. Set by
    /// [`atomic`] before running its closure; read by [`on_commit`]
    /// from anywhere inside that closure's call tree.
    static ON_COMMIT: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>>;
}

/// Closure-scoped transaction with after-commit hooks. Django's
/// [`transaction.atomic`](https://docs.djangoproject.com/en/6.0/topics/db/transactions/#django.db.transaction.atomic)
/// + [`transaction.on_commit`](https://docs.djangoproject.com/en/6.0/topics/db/transactions/#performing-actions-after-commit),
/// rolled into one helper. Auto-commits when `f` returns `Ok`,
/// auto-rolls-back when `f` returns `Err`. Callbacks queued via
/// [`on_commit`] inside `f` fire **only on the commit path** —
/// never on rollback.
///
/// Without this guarantee, side effects like "send the welcome
/// email" after an `INSERT` can leak: the email goes out, the
/// transaction rolls back, the user record never lands, the email
/// references a phantom user.
///
/// ```ignore
/// use rustango::sql::{atomic, on_commit, insert_tx};
///
/// atomic(&pool, |tx| Box::pin(async move {
///     insert_tx(tx, &user_insert).await?;
///     on_commit(|| {
///         // Sync. For async work, spawn here.
///         tokio::spawn(async move { send_welcome_email(user_id).await });
///     });
///     Ok(())
/// }))
/// .await?;
/// ```
///
/// The `Box::pin(async move { … })` wrapping is the cost of an async
/// closure that borrows `tx` mutably across `await` points on stable
/// Rust — `&mut PoolTx<'_>` is lifetime-invariant, and `Pin<Box<dyn
/// Future>>` is the standard escape hatch. The [`atomic!`] macro
/// hides the ceremony if you prefer:
///
/// ```ignore
/// rustango::atomic!(&pool, |tx| {
///     insert_tx(tx, &user_insert).await?;
///     on_commit(|| { /* … */ });
///     Ok(())
/// })
/// .await?;
/// ```
///
/// **Inside the closure** `tx` is `&mut PoolTx<'_>` — pass directly
/// to the existing `_tx` helpers (`insert_tx` / `update_tx` /
/// `select_rows_tx_with_related` / ...). Raw `sqlx::query` chains
/// still need the per-backend `PoolTx::Postgres(...)` match (that's
/// the escape hatch); typed ORM ops dispatch internally.
///
/// **Callbacks fire in registration order**, serially, after the
/// `COMMIT` returns OK. A panicking callback aborts the chain —
/// subsequent callbacks won't run. Wrap in `std::panic::catch_unwind`
/// if you need per-callback resilience.
///
/// # Errors
/// Returns the first `ExecError` produced by `f`, or a driver error
/// from `BEGIN` / `COMMIT` / `ROLLBACK`.
pub async fn atomic<F, T>(pool: &Pool, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx mut PoolTx<'_>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let queue = std::sync::Mutex::new(Vec::<Box<dyn FnOnce() + Send>>::new());
    ON_COMMIT
        .scope(queue, async move {
            let mut tx = transaction_pool(pool).await?;
            match f(&mut tx).await {
                Ok(val) => {
                    tx.commit().await?;
                    // Drain queue + fire callbacks in registration order.
                    let callbacks = ON_COMMIT
                        .with(|q| std::mem::take(&mut *q.lock().expect("on_commit mutex")));
                    for cb in callbacks {
                        cb();
                    }
                    Ok(val)
                }
                Err(e) => {
                    // Callbacks drop here when the task-local scope ends.
                    let _ = tx.rollback().await;
                    Err(e)
                }
            }
        })
        .await
}

/// Sugar over [`atomic`] that wraps the body in `Box::pin(async move { … })`
/// so callers don't have to. Identical semantics:
///
/// ```ignore
/// rustango::atomic!(&pool, |tx| {
///     insert_tx(tx, &q).await?;
///     on_commit(|| spawn_email());
///     Ok(())
/// })
/// .await?;
/// ```
#[macro_export]
macro_rules! atomic {
    ($pool:expr, |$tx:ident| $body:block) => {
        async {
            // Clone the pool into a local so it stays alive for the
            // full future, even when nested inside an outer `async
            // move` block (which would otherwise try to move the
            // caller's `pool` binding through this scope). `Pool` is
            // cheap-clone (Arc-based) so this is a zero-cost
            // ergonomic shim.
            let __rustango_atomic_pool = ::core::clone::Clone::clone($pool);
            $crate::sql::atomic(&__rustango_atomic_pool, |$tx| {
                ::std::boxed::Box::pin(async move { $body })
            })
            .await
        }
    };
}

/// Queue `f` to run after the enclosing [`atomic`] block commits. If
/// the transaction rolls back instead, `f` is dropped unfired.
///
/// `f` is sync (`FnOnce() + Send + 'static`). For async work, spawn
/// from inside:
///
/// ```ignore
/// on_commit(|| {
///     tokio::spawn(async move { send_email().await });
/// });
/// ```
///
/// Calling `on_commit` **outside** an `atomic` scope is a programmer
/// error and panics with a clear message — flash-fail beats silently
/// dropping the callback into the void.
pub fn on_commit<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    ON_COMMIT
        .try_with(|q| {
            q.lock().expect("on_commit mutex").push(Box::new(f));
        })
        .unwrap_or_else(|_| {
            panic!(
                "rustango::sql::on_commit called outside an `atomic` block — \
                 the callback would never fire. Wrap the caller in \
                 `atomic(&pool, |tx| async move {{ ... on_commit(...) ... }})`."
            );
        });
}

/// Returns the number of callbacks queued in the current `atomic`
/// scope. Useful for tests. Returns 0 when called outside an
/// `atomic` block.
#[must_use]
pub fn on_commit_pending() -> usize {
    ON_COMMIT
        .try_with(|q| q.lock().expect("on_commit mutex").len())
        .unwrap_or(0)
}
