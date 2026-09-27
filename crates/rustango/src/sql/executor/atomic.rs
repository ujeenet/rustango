//! `atomic()` + `on_commit()` — a closure-scoped transaction and its
//! after-commit hooks, rolled into one helper. Issue #44.
//!
//! The outermost block owns the transaction in a task-local slot; a
//! nested block on the same pool finds it and runs in a savepoint (#1666).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::{transaction_pool, ExecError, PoolTx};
use crate::sql::Pool;

type Callback = Box<dyn FnOnce() + Send>;

/// The transaction of one outermost [`atomic`] block.
struct Slot {
    pool: usize,
    state: tokio::sync::Mutex<TxState>,
    /// Shallowest savepoint whose block was dropped mid-flight (0 = none).
    cancelled: AtomicUsize,
}

struct TxState {
    tx: Option<PoolTx<'static>>,
    /// Savepoints currently open.
    depth: usize,
}

impl Slot {
    fn mark_cancelled(&self, depth: usize) {
        let _ = self
            .cancelled
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
                Some(if c == 0 { depth } else { c.min(depth) })
            });
    }
}

impl TxState {
    fn tx(&mut self) -> &mut PoolTx<'static> {
        self.tx.as_mut().expect("atomic transaction is open")
    }

    async fn savepoint(&mut self, verb: &str, depth: usize) -> Result<(), ExecError> {
        let tx = self.tx();
        let name = tx.dialect().quote_ident(&format!("rustango_sp_{depth}"));
        tx.execute_unprepared(&format!("{verb} {name}")).await?;
        Ok(())
    }

    /// Roll back a savepoint whose block was cancelled, before any other use.
    async fn settle(&mut self, slot: &Slot) -> Result<(), ExecError> {
        let d = slot.cancelled.swap(0, Ordering::SeqCst);
        if d != 0 && d <= self.depth {
            if let Err(e) = self.rollback_to(d).await {
                slot.mark_cancelled(d);
                return Err(e);
            }
        }
        Ok(())
    }

    async fn rollback_to(&mut self, depth: usize) -> Result<(), ExecError> {
        self.savepoint("ROLLBACK TO SAVEPOINT", depth).await?;
        self.savepoint("RELEASE SAVEPOINT", depth).await?;
        self.depth = depth - 1;
        Ok(())
    }
}

/// Handle an [`atomic`] block gets. Lock it for each statement; drop
/// the guard before a nested `atomic()`.
pub struct AtomicTx {
    slot: Arc<Slot>,
}

impl AtomicTx {
    /// Lock the transaction. The guard derefs to [`PoolTx`], so the
    /// `_tx` helpers take `&mut *guard`.
    ///
    /// # Errors
    /// A driver error rolling back a cancelled nested block.
    pub async fn lock(&self) -> Result<TxGuard<'_>, ExecError> {
        let mut st = self.slot.state.lock().await;
        st.settle(&self.slot).await?;
        Ok(TxGuard(st))
    }
}

/// Exclusive use of an [`atomic`] block's transaction, from [`AtomicTx::lock`].
pub struct TxGuard<'a>(tokio::sync::MutexGuard<'a, TxState>);

impl std::ops::Deref for TxGuard<'_> {
    type Target = PoolTx<'static>;
    fn deref(&self) -> &PoolTx<'static> {
        self.0.tx.as_ref().expect("atomic transaction is open")
    }
}

impl std::ops::DerefMut for TxGuard<'_> {
    fn deref_mut(&mut self) -> &mut PoolTx<'static> {
        self.0.tx()
    }
}

/// Open transactions and callback levels of one task.
#[derive(Default)]
struct Scope {
    slots: Vec<Arc<Slot>>,
    levels: Vec<Level>,
}

/// `on_commit` callbacks queued by one block.
struct Level {
    pool: usize,
    callbacks: Vec<Callback>,
}

tokio::task_local! {
    /// Set by the first [`atomic`] in a task; nested blocks reuse it.
    static ON_COMMIT: Mutex<Scope>;
}

fn with_scope<R>(f: impl FnOnce(&mut Scope) -> R) -> Option<R> {
    ON_COMMIT
        .try_with(|s| f(&mut s.lock().unwrap_or_else(PoisonError::into_inner)))
        .ok()
}

/// Identity of a pool; clones of one pool share it.
fn pool_key(pool: &Pool) -> usize {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => Arc::as_ptr(&p.connect_options()) as usize,
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => Arc::as_ptr(&p.connect_options()) as usize,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => Arc::as_ptr(&p.connect_options()) as usize,
    }
}

/// A block's callback level. Dropped untaken, its callbacks drop too.
struct LevelGuard(Option<usize>);

impl LevelGuard {
    fn open(pool: usize) -> Self {
        Self(with_scope(|s| {
            s.levels.push(Level {
                pool,
                callbacks: Vec::new(),
            });
            s.levels.len() - 1
        }))
    }

    fn take(mut self) -> Vec<Callback> {
        self.0
            .take()
            .and_then(|at| with_scope(|s| s.levels.drain(at..).flat_map(|l| l.callbacks).collect()))
            .unwrap_or_default()
    }
}

impl Drop for LevelGuard {
    fn drop(&mut self) {
        if let Some(at) = self.0 {
            with_scope(|s| s.levels.truncate(at));
        }
    }
}

/// Unregisters an outermost block's slot, however the block ends.
struct SlotGuard(Arc<Slot>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        with_scope(|s| s.slots.retain(|x| !Arc::ptr_eq(x, &self.0)));
    }
}

/// An open savepoint. Dropped before it ends, it is rolled back on next use.
struct OpenSavepoint {
    slot: Arc<Slot>,
    depth: usize,
    ended: bool,
}

impl Drop for OpenSavepoint {
    fn drop(&mut self) {
        if !self.ended {
            self.slot.mark_cancelled(self.depth);
        }
    }
}

/// Closure-scoped transaction with after-commit hooks.
/// Auto-commits when `f` returns `Ok`,
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
///     insert_tx(&mut *tx.lock().await?, &user_insert).await?;
///     on_commit(|| {
///         // Sync. For async work, spawn here.
///         tokio::spawn(async move { send_welcome_email(user_id).await });
///     });
///     Ok(())
/// }))
/// .await?;
/// ```
///
/// The [`atomic!`](crate::atomic) macro hides the `Box::pin` ceremony.
///
/// **Inside the closure** `tx` is an [`AtomicTx`]: lock it per
/// statement and pass `&mut *guard` to the `_tx` helpers, or match the
/// guard's [`PoolTx`] variant for raw sqlx.
///
/// **Nesting:** `atomic` on the same pool inside the block runs in a
/// savepoint on the same connection. Its `Err` rolls back only its own
/// writes; its callbacks wait for the outermost commit. Holding a
/// [`TxGuard`] across the nested call returns
/// [`ExecError::NestedAtomic`]. A different pool gets its own
/// transaction. A dropped (cancelled) block is rolled back.
///
/// **Spawned tasks** do not inherit the block: `atomic` inside
/// `tokio::spawn` opens a separate transaction, and `on_commit` there
/// panics.
///
/// **Callbacks fire in registration order**, serially, after the
/// `COMMIT` returns OK. A panicking callback aborts the chain.
///
/// # Errors
/// Returns the first `ExecError` produced by `f`, or a driver error
/// from `BEGIN` / `SAVEPOINT` / `COMMIT` / `ROLLBACK`.
pub async fn atomic<F, T>(pool: &Pool, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    if ON_COMMIT.try_with(|_| ()).is_ok() {
        run(pool, f).await
    } else {
        ON_COMMIT
            .scope(Mutex::new(Scope::default()), run(pool, f))
            .await
    }
}

async fn run<F, T>(pool: &Pool, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let key = pool_key(pool);
    let open = with_scope(|s| s.slots.iter().rev().find(|x| x.pool == key).cloned()).flatten();
    match open {
        Some(slot) => nested(slot, f).await,
        None => outermost(pool, key, f).await,
    }
}

async fn outermost<F, T>(pool: &Pool, key: usize, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let slot = Arc::new(Slot {
        pool: key,
        state: tokio::sync::Mutex::new(TxState {
            tx: Some(transaction_pool(pool).await?),
            depth: 0,
        }),
        cancelled: AtomicUsize::new(0),
    });
    with_scope(|s| s.slots.push(Arc::clone(&slot)));
    let _registered = SlotGuard(Arc::clone(&slot));
    let level = LevelGuard::open(key);
    let handle = AtomicTx {
        slot: Arc::clone(&slot),
    };
    let res = f(&handle).await;
    let mut st = slot.state.lock().await;
    let settled = st.settle(&slot).await;
    let tx = st.tx.take().expect("atomic transaction is open");
    drop(st);
    match res.and_then(|v| settled.map(|()| v)) {
        Ok(v) => {
            tx.commit().await?;
            for cb in level.take() {
                cb();
            }
            Ok(v)
        }
        Err(e) => {
            // Callbacks drop with `level`.
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

async fn nested<F, T>(slot: Arc<Slot>, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let depth = {
        let mut st = slot.state.try_lock().map_err(|_| ExecError::NestedAtomic)?;
        st.settle(&slot).await?;
        let depth = st.depth + 1;
        st.savepoint("SAVEPOINT", depth).await?;
        st.depth = depth;
        depth
    };
    let mut open = OpenSavepoint {
        slot: Arc::clone(&slot),
        depth,
        ended: false,
    };
    let level = LevelGuard::open(slot.pool);
    let handle = AtomicTx {
        slot: Arc::clone(&slot),
    };
    let res = f(&handle).await;
    let mut st = slot.state.lock().await;
    st.settle(&slot).await?;
    match res {
        Ok(v) => {
            st.savepoint("RELEASE SAVEPOINT", depth).await?;
            st.depth = depth - 1;
            open.ended = true;
            let callbacks = level.take();
            with_scope(|s| {
                if let Some(parent) = s.levels.iter_mut().rev().find(|l| l.pool == slot.pool) {
                    parent.callbacks.extend(callbacks);
                }
            });
            Ok(v)
        }
        Err(e) => {
            // On failure `open` stays unended, so the next use retries.
            if st.rollback_to(depth).await.is_ok() {
                open.ended = true;
            }
            Err(e)
        }
    }
}

/// Sugar over [`atomic`] that wraps the body in `Box::pin(async move { … })`
/// so callers don't have to. Identical semantics:
///
/// ```ignore
/// rustango::atomic!(&pool, |tx| {
///     insert_tx(&mut *tx.lock().await?, &q).await?;
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

/// Queue `f` to run after the outermost [`atomic`] block commits. If
/// that transaction, or an enclosing nested block, rolls back instead,
/// `f` is dropped unfired.
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
/// dropping the callback into the void. Called from a callback after
/// the commit, it runs at once.
pub fn on_commit<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let now = ON_COMMIT
        .try_with(move |s| {
            let mut s = s.lock().unwrap_or_else(PoisonError::into_inner);
            match s.levels.last_mut() {
                Some(level) => {
                    level.callbacks.push(Box::new(f));
                    None
                }
                None => Some(f),
            }
        })
        .unwrap_or_else(|_| {
            panic!(
                "rustango::sql::on_commit called outside an `atomic` block — \
                 the callback would never fire. Wrap the caller in \
                 `atomic(&pool, |tx| async move {{ ... on_commit(...) ... }})`."
            );
        });
    if let Some(f) = now {
        f();
    }
}

/// Returns the number of callbacks queued in the current `atomic`
/// scope. Useful for tests. Returns 0 when called outside an
/// `atomic` block.
#[must_use]
pub fn on_commit_pending() -> usize {
    with_scope(|s| s.levels.iter().map(|l| l.callbacks.len()).sum()).unwrap_or(0)
}
