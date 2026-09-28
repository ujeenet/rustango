//! `atomic()` + `on_commit()` — a closure-scoped transaction and its
//! after-commit hooks, rolled into one helper. Issue #44.
//!
//! The outermost block owns the transaction; a nested block on the same
//! pool runs in a savepoint on it (#1666). Each block's future carries its
//! own context in a task-local, so `join!`ed blocks cannot see each other's.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::OwnedMutexGuard;

use super::{transaction_pool, ExecError, PoolTx};
use crate::sql::Pool;

type Callback = Box<dyn FnOnce() + Send>;

/// Identity of a pool: the options `Arc` its clones share. Held, so the
/// address cannot be reused while a block is open.
#[derive(Clone)]
struct PoolId(Arc<dyn Any + Send + Sync>);

impl PoolId {
    fn of(pool: &Pool) -> Self {
        match pool {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => Self(p.connect_options()),
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => Self(p.connect_options()),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => Self(p.connect_options()),
        }
    }

    fn same(&self, other: &Self) -> bool {
        std::ptr::eq(
            Arc::as_ptr(&self.0).cast::<()>(),
            Arc::as_ptr(&other.0).cast::<()>(),
        )
    }
}

/// The transaction of one outermost block.
struct Slot {
    state: Arc<tokio::sync::Mutex<TxState>>,
    /// Shallowest savepoint whose block was dropped mid-flight (0 = none).
    cancelled: AtomicUsize,
    /// Live [`TxGuard`]s; a busy lock with none is a background finisher.
    guards: AtomicUsize,
}

struct TxState {
    tx: Option<PoolTx<'static>>,
    /// Savepoints currently open.
    depth: usize,
    /// A savepoint statement failed; the transaction can only roll back.
    poisoned: bool,
}

impl TxState {
    fn tx(&mut self) -> &mut PoolTx<'static> {
        self.tx.as_mut().expect("atomic transaction is open")
    }

    async fn savepoint(&mut self, verb: &str, depth: usize) -> Result<(), ExecError> {
        let tx = self.tx();
        let name = tx.dialect().quote_ident(&format!("rustango_sp_{depth}"));
        let r = tx.execute_unprepared(&format!("{verb} {name}")).await;
        if r.is_err() {
            self.poisoned = true;
        }
        Ok(r?)
    }

    async fn rollback_to(&mut self, depth: usize) -> Result<(), ExecError> {
        self.savepoint("ROLLBACK TO SAVEPOINT", depth).await?;
        self.savepoint("RELEASE SAVEPOINT", depth).await?;
        self.depth = depth - 1;
        Ok(())
    }

    /// Refuse a poisoned transaction; roll back a cancelled block first.
    async fn settle(&mut self, slot: &Slot) -> Result<(), ExecError> {
        if self.poisoned {
            return Err(ExecError::AtomicAborted);
        }
        let d = slot.cancelled.swap(0, Ordering::SeqCst);
        if d != 0 && d <= self.depth {
            self.rollback_to(d).await?;
        }
        Ok(())
    }
}

/// Run `fut` to completion even if the caller is dropped, so a savepoint
/// statement is never cut off halfway.
async fn in_background<T: Send + 'static>(
    fut: impl Future<Output = Result<T, ExecError>> + Send + 'static,
) -> Result<T, ExecError> {
    match tokio::spawn(fut).await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(ExecError::AtomicAborted),
    }
}

/// Lock the slot, settled. A lock held by a live guard is misuse, not a wait.
async fn acquire(slot: &Arc<Slot>) -> Result<OwnedMutexGuard<TxState>, ExecError> {
    let guard = match Arc::clone(&slot.state).try_lock_owned() {
        Ok(g) => g,
        Err(_) if slot.guards.load(Ordering::SeqCst) > 0 => return Err(ExecError::NestedAtomic),
        Err(_) => Arc::clone(&slot.state).lock_owned().await,
    };
    if !guard.poisoned && slot.cancelled.load(Ordering::SeqCst) == 0 {
        return Ok(guard);
    }
    let slot = Arc::clone(slot);
    in_background(async move {
        let mut g = guard;
        g.settle(&slot).await.map(|()| g)
    })
    .await
}

/// One `atomic` call, as seen by the code inside it.
struct Block {
    pool: PoolId,
    slot: Arc<Slot>,
    /// Savepoint depth this block writes at (0 = the transaction itself).
    depth: usize,
    /// The block this one was called from, on any pool.
    enclosing: Option<Arc<Block>>,
    callbacks: Mutex<Vec<Callback>>,
    /// Set after the outermost commit: `on_commit` then runs at once.
    committed: AtomicBool,
}

impl Block {
    fn take_callbacks(&self) -> Vec<Callback> {
        std::mem::take(
            &mut self
                .callbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

tokio::task_local! {
    /// The innermost block around the running future.
    static BLOCK: Arc<Block>;
}

/// Handle an [`atomic`] block gets. Lock it for each statement; drop
/// the guard before a nested `atomic()`.
pub struct AtomicTx {
    block: Arc<Block>,
}

impl AtomicTx {
    /// Lock the transaction. The guard derefs to [`PoolTx`], so the
    /// `_tx` helpers take `&mut *guard`.
    ///
    /// # Errors
    /// [`ExecError::NestedAtomic`] while a guard is held or a nested block
    /// is open; [`ExecError::AtomicAborted`] after a failed savepoint.
    pub async fn lock(&self) -> Result<TxGuard<'_>, ExecError> {
        let slot = &self.block.slot;
        let guard = acquire(slot).await?;
        if guard.depth != self.block.depth {
            return Err(ExecError::NestedAtomic);
        }
        slot.guards.fetch_add(1, Ordering::SeqCst);
        Ok(TxGuard {
            guard,
            slot: Arc::clone(slot),
            _handle: PhantomData,
        })
    }
}

/// Exclusive use of an [`atomic`] block's transaction, from [`AtomicTx::lock`].
pub struct TxGuard<'a> {
    guard: OwnedMutexGuard<TxState>,
    slot: Arc<Slot>,
    _handle: PhantomData<&'a AtomicTx>,
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        self.slot.guards.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::ops::Deref for TxGuard<'_> {
    type Target = PoolTx<'static>;
    fn deref(&self) -> &PoolTx<'static> {
        self.guard.tx.as_ref().expect("atomic transaction is open")
    }
}

impl std::ops::DerefMut for TxGuard<'_> {
    fn deref_mut(&mut self) -> &mut PoolTx<'static> {
        self.guard.tx()
    }
}

/// An open savepoint. Dropped before its end starts, it is rolled back on next use.
struct OpenSavepoint {
    slot: Arc<Slot>,
    depth: usize,
    ending: bool,
}

impl Drop for OpenSavepoint {
    fn drop(&mut self) {
        if !self.ending {
            let _ = self
                .slot
                .cancelled
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
                    Some(if c == 0 {
                        self.depth
                    } else {
                        c.min(self.depth)
                    })
                });
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
/// ```no_run
/// # async fn demo(pool: &rustango::sql::Pool, q: &rustango::core::InsertQuery)
/// # -> Result<(), rustango::sql::ExecError> {
/// use rustango::sql::{atomic, insert_tx, on_commit};
///
/// // The closures capture owned values: they must not borrow the caller.
/// let (p, q1, q2) = (pool.clone(), q.clone(), q.clone());
/// atomic(pool, move |tx| Box::pin(async move {
///     insert_tx(&mut *tx.lock().await?, &q1).await?;
///     // Nested: a savepoint on the same connection.
///     let _ = atomic(&p, move |sp| Box::pin(async move {
///         insert_tx(&mut *sp.lock().await?, &q2).await
///     }))
///     .await; // its failure keeps the first row
///     on_commit(|| println!("committed"));
///     Ok(())
/// }))
/// .await
/// # }
/// ```
///
/// The [`atomic!`](crate::atomic) macro hides the `Box::pin` ceremony.
///
/// **Inside the closure** `tx` is an [`AtomicTx`]: lock it per
/// statement and pass `&mut *guard` to the `_tx` helpers, or match the
/// guard's [`PoolTx`] variant for raw sqlx.
///
/// **Nesting** is per pool object: `atomic` on the same `Pool` (or a
/// clone) inside the block runs in a savepoint on the same connection.
/// Pass the request's pool down rather than looking it up again. Its
/// `Err` rolls back only its own writes; its callbacks wait for the
/// outermost commit. A different pool gets its own transaction. A
/// dropped (cancelled) or panicking nested block is rolled back. Two
/// nested blocks on one transaction at once (`join!`), or a
/// [`TxGuard`] held across a nested call, return
/// [`ExecError::NestedAtomic`]. If a savepoint statement fails, the
/// whole transaction rolls back and [`ExecError::AtomicAborted`] is
/// returned.
///
/// **Costs:** past 64 open savepoints in one transaction PostgreSQL
/// spills its subtransaction cache, so avoid nesting in hot loops. On
/// MySQL a deadlock inside a nested block rolls back the whole outer
/// transaction.
///
/// **Spawned tasks** do not inherit the block: `atomic` inside
/// `tokio::spawn` opens a separate transaction (a real, independent
/// commit), and `on_commit` there panics.
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
    let id = PoolId::of(pool);
    let current = BLOCK.try_with(Arc::clone).ok();
    let same_pool =
        std::iter::successors(current.clone(), |b| b.enclosing.clone()).find(|b| b.pool.same(&id));
    match same_pool {
        Some(parent) => nested(parent, current, f).await,
        None => outermost(pool, id, current, f).await,
    }
}

async fn outermost<F, T>(
    pool: &Pool,
    id: PoolId,
    enclosing: Option<Arc<Block>>,
    f: F,
) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let slot = Arc::new(Slot {
        state: Arc::new(tokio::sync::Mutex::new(TxState {
            tx: Some(transaction_pool(pool).await?),
            depth: 0,
            poisoned: false,
        })),
        cancelled: AtomicUsize::new(0),
        guards: AtomicUsize::new(0),
    });
    let block = Arc::new(Block {
        pool: id,
        slot: Arc::clone(&slot),
        depth: 0,
        enclosing,
        callbacks: Mutex::new(Vec::new()),
        committed: AtomicBool::new(false),
    });
    let handle = AtomicTx {
        block: Arc::clone(&block),
    };
    let res = BLOCK.scope(Arc::clone(&block), f(&handle)).await;
    // Waits for any background savepoint work, then rolls back a
    // cancelled nested block before deciding.
    let mut st = slot.state.lock().await;
    let settled = match st.settle(&slot).await {
        Ok(()) if st.depth != 0 => Err(ExecError::AtomicAborted),
        other => other,
    };
    let tx = st.tx.take().expect("atomic transaction is open");
    drop(st);
    match res.and_then(|v| settled.map(|()| v)) {
        Ok(v) => {
            tx.commit().await?;
            block.committed.store(true, Ordering::SeqCst);
            let callbacks = block.take_callbacks();
            BLOCK.sync_scope(block, || {
                for cb in callbacks {
                    cb();
                }
            });
            Ok(v)
        }
        Err(e) => {
            // Callbacks drop with `block`.
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

async fn nested<F, T>(
    parent: Arc<Block>,
    enclosing: Option<Arc<Block>>,
    f: F,
) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let slot = Arc::clone(&parent.slot);
    let guard = acquire(&slot).await?;
    let parent_depth = parent.depth;
    let depth = in_background(async move {
        let mut g = guard;
        // Only the innermost open block may open a child.
        if g.depth != parent_depth {
            return Err(ExecError::NestedAtomic);
        }
        g.savepoint("SAVEPOINT", parent_depth + 1).await?;
        g.depth = parent_depth + 1;
        Ok(g.depth)
    })
    .await?;
    let mut open = OpenSavepoint {
        slot: Arc::clone(&slot),
        depth,
        ending: false,
    };
    let block = Arc::new(Block {
        pool: parent.pool.clone(),
        slot: Arc::clone(&slot),
        depth,
        enclosing,
        callbacks: Mutex::new(Vec::new()),
        committed: AtomicBool::new(false),
    });
    let handle = AtomicTx {
        block: Arc::clone(&block),
    };
    let res = BLOCK.scope(Arc::clone(&block), f(&handle)).await;
    let guard = match acquire(&slot).await {
        Ok(g) => g,
        // `open` is still armed: the next use rolls this savepoint back.
        Err(e) => return Err(res.err().unwrap_or(e)),
    };
    // From here the background task finishes the savepoint either way.
    open.ending = true;
    let release = res.is_ok();
    let ended = in_background(async move {
        let mut g = guard;
        if g.depth != depth {
            g.poisoned = true;
            return Err(ExecError::AtomicAborted);
        }
        if release {
            g.savepoint("RELEASE SAVEPOINT", depth).await?;
            g.depth = depth - 1;
            Ok(())
        } else {
            g.rollback_to(depth).await
        }
    })
    .await;
    // A block's own error wins over a failure to end its savepoint.
    let v = res?;
    ended?;
    let callbacks = block.take_callbacks();
    parent
        .callbacks
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .extend(callbacks);
    Ok(v)
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
    let block = BLOCK.try_with(Arc::clone).unwrap_or_else(|_| {
        panic!(
            "rustango::sql::on_commit called outside an `atomic` block — \
             the callback would never fire. Wrap the caller in \
             `atomic(&pool, |tx| async move {{ ... on_commit(...) ... }})`."
        );
    });
    if block.committed.load(Ordering::SeqCst) {
        f();
    } else {
        block
            .callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Box::new(f));
    }
}

/// Returns the number of callbacks queued in the current `atomic`
/// block and the blocks around it. Useful for tests. Returns 0 when
/// called outside an `atomic` block.
#[must_use]
pub fn on_commit_pending() -> usize {
    std::iter::successors(BLOCK.try_with(Arc::clone).ok(), |b| b.enclosing.clone())
        .map(|b| {
            b.callbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
        })
        .sum()
}
