//! `atomic()` + `on_commit()` — a closure-scoped transaction and its
//! after-commit hooks, rolled into one helper. Issue #44.
//!
//! The outermost block owns the transaction; a nested block on the same
//! pool runs in a savepoint on it (#1666). Each block's future carries its
//! own context in a task-local, so `join!`ed blocks cannot see each other's.

use std::collections::BTreeSet;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::OwnedMutexGuard;

use super::{transaction_pool, ExecError, PoolTx};
use crate::sql::Pool;

type Callback = Box<dyn FnOnce() + Send>;

/// Identity of a pool: the address of the pool state its clones share.
/// The clone held here keeps that address from being reused, and unlike
/// `connect_options()` it survives `set_connect_options`.
#[derive(Clone)]
struct PoolId(Pool);

impl PoolId {
    fn key(&self) -> *const () {
        match &self.0 {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => std::ptr::from_ref(p.options()).cast(),
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => std::ptr::from_ref(p.options()).cast(),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => std::ptr::from_ref(p.options()).cast(),
        }
    }

    fn same(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

/// The transaction of one outermost block.
struct Slot {
    state: Arc<tokio::sync::Mutex<TxState>>,
    /// Shallowest savepoint whose block was dropped mid-flight (0 = none).
    cancelled: AtomicUsize,
    /// Live [`TxGuard`]s.
    guards: AtomicUsize,
    /// Background savepoint work whose caller still waits for it.
    claims: AtomicUsize,
    /// A statement error (MySQL) or an automatic rollback (SQLite) ended
    /// the transaction on the server.
    fatal: Arc<AtomicBool>,
    /// Address of the `PoolTx` inside `state`, so ORM statements find it.
    tx_key: usize,
}

/// Counts a caller waiting on background savepoint work, for its lifetime.
struct Claim<'a>(&'a Slot);

impl<'a> Claim<'a> {
    fn new(slot: &'a Slot) -> Self {
        slot.claims.fetch_add(1, Ordering::SeqCst);
        Self(slot)
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.claims.fetch_sub(1, Ordering::SeqCst);
    }
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

    /// Poison the transaction if the server already ended it.
    async fn check_open(&mut self, slot: &Slot) -> Result<(), ExecError> {
        if self.tx().still_open().await {
            return Ok(());
        }
        self.poisoned = true;
        // No failed statement seen: the server ended it on its own, e.g.
        // a MySQL DDL implicit commit, so some writes may be committed.
        let mysql = self.tx().dialect().name() == "mysql";
        Err(if mysql && !slot.fatal.load(Ordering::SeqCst) {
            ExecError::AtomicEndedEarly
        } else {
            ExecError::AtomicAborted
        })
    }

    /// Refuse a poisoned transaction; roll back a cancelled block first.
    async fn settle(&mut self, slot: &Slot) -> Result<(), ExecError> {
        if self.poisoned || slot.fatal.load(Ordering::SeqCst) {
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

/// Lock the slot, settled. A lock held or claimed by a live caller is
/// misuse (`join!`), not a wait; only an orphaned finisher is waited for.
async fn acquire(slot: &Arc<Slot>) -> Result<OwnedMutexGuard<TxState>, ExecError> {
    let busy =
        |s: &Slot| s.guards.load(Ordering::SeqCst) > 0 || s.claims.load(Ordering::SeqCst) > 0;
    // Poll rather than queue: a queued waiter could not be refused if a
    // live caller takes the lock first, and would wait on it forever.
    let guard = loop {
        match Arc::clone(&slot.state).try_lock_owned() {
            Ok(g) => break g,
            Err(_) if busy(slot) => return Err(ExecError::NestedAtomic),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
        }
    };
    if !guard.poisoned
        && !slot.fatal.load(Ordering::SeqCst)
        && slot.cancelled.load(Ordering::SeqCst) == 0
    {
        return Ok(guard);
    }
    let _claim = Claim::new(slot);
    let task_slot = Arc::clone(slot);
    in_background(async move {
        let mut g = guard;
        g.settle(&task_slot).await?;
        Ok(g)
    })
    .await
}

/// Refuses ORM statements on a transaction a statement error ended.
pub(crate) struct Gate(Option<Arc<Slot>>);

/// Look up the `atomic` transaction `tx` belongs to, and refuse to run on it
/// once the server ended it. Called by every `_tx` statement helper.
pub(crate) async fn gate(tx: &PoolTx<'_>) -> Result<Gate, ExecError> {
    let key = std::ptr::from_ref::<PoolTx<'_>>(tx) as usize;
    let slot = BLOCK
        .try_with(|b| {
            std::iter::successors(Some(Arc::clone(b)), |b| b.enclosing.clone())
                .map(|b| Arc::clone(&b.slot))
                .find(|s| s.tx_key == key)
        })
        .ok()
        .flatten();
    if slot
        .as_ref()
        .is_some_and(|s| s.fatal.load(Ordering::SeqCst))
    {
        return Err(ExecError::AtomicAborted);
    }
    Ok(Gate(slot))
}

impl Gate {
    /// Record a statement error that ended the whole transaction.
    pub(crate) fn check<T>(&self, r: Result<T, ExecError>) -> Result<T, ExecError> {
        if let (Some(slot), Err(ExecError::Driver(e))) = (&self.0, &r) {
            if ends_transaction(e) {
                slot.fatal.store(true, Ordering::SeqCst);
            }
        }
        r
    }
}

/// MySQL rolls the whole transaction back on a deadlock (1213) and may on a
/// lock-wait timeout (1205); a lost connection ends it everywhere.
fn ends_transaction(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => true,
        #[cfg(feature = "mysql")]
        sqlx::Error::Database(db) => db
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .is_some_and(|m| matches!(m.number(), 1213 | 1205)),
        _ => false,
    }
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
    /// `&Pool` calls this block already warned about.
    warned: Mutex<BTreeSet<&'static str>>,
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

/// Hand-off between a nested block and its background SAVEPOINT task, so
/// a block dropped while its savepoint opens is still rolled back.
#[derive(Default)]
struct Opening {
    opened: AtomicBool,
    abandoned: AtomicBool,
}

/// An open savepoint. Dropped before its end starts, it is rolled back on next use.
struct OpenSavepoint {
    slot: Arc<Slot>,
    depth: usize,
    opening: Arc<Opening>,
    ending: bool,
}

fn mark_cancelled(slot: &Slot, depth: usize) {
    let _ = slot
        .cancelled
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
            Some(if c == 0 { depth } else { c.min(depth) })
        });
}

impl Drop for OpenSavepoint {
    fn drop(&mut self) {
        if !self.ending {
            // Either side that sees both flags marks it; a refused open never does.
            self.opening.abandoned.store(true, Ordering::SeqCst);
            if self.opening.opened.load(Ordering::SeqCst) {
                mark_cancelled(&self.slot, self.depth);
            }
        }
    }
}

/// Whether the running task is inside an [`atomic`] block on `pool`.
pub(crate) fn in_block(pool: &Pool) -> bool {
    let id = PoolId(pool.clone());
    BLOCK
        .try_with(|b| {
            std::iter::successors(Some(Arc::clone(b)), |b| b.enclosing.clone())
                .any(|b| b.pool.same(&id))
        })
        .unwrap_or(false)
}

/// Warn, once per `op` and block, that `op` ran on `pool` while an
/// [`atomic`] block on it is open: it takes another connection (#1460).
pub(crate) fn warn_if_in_block(pool: &Pool, op: &'static str) {
    let id = PoolId(pool.clone());
    let first = BLOCK
        .try_with(|b| {
            std::iter::successors(Some(Arc::clone(b)), |b| b.enclosing.clone())
                .find(|b| b.pool.same(&id))
                .is_some_and(|b| {
                    b.warned
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(op)
                })
        })
        .unwrap_or(false);
    if first {
        tracing::warn!(
            target: "rustango::atomic",
            op,
            "a `&Pool` call ran inside an `atomic` block on the same pool: it uses \
             another connection, so it does not see the block's writes and can \
             deadlock a small pool; use the `_tx` helper on the block's guard"
        );
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
/// **`&Pool` calls inside the block** (`fetch(&pool)`, `save_pool`, …) run
/// on another connection, outside the block, and can deadlock a small
/// pool. Each kind logs one `rustango::atomic` warning per block; use the `_tx` helpers.
///
/// **Server-ended transactions:** after a statement error that ends the
/// whole transaction (MySQL deadlock 1213 / timeout 1205, SQLite automatic
/// rollback, a lost connection) every later ORM statement in the block
/// returns [`ExecError::AtomicAborted`], and so does `atomic`. Before the
/// outermost COMMIT the block also checks the transaction is still open
/// (one round trip on PG, two on MySQL): a PG error the closure ignored
/// returns `AtomicAborted` instead of a silent rollback. Raw sqlx on the
/// guard bypasses the per-statement check.
///
/// **MySQL implicit commits:** DDL, `TRUNCATE` and `LOCK TABLES` commit
/// the transaction. `atomic` then returns [`ExecError::AtomicEndedEarly`]
/// with some writes already committed; do not blindly retry.
///
/// **Failed statements differ by backend:** PG aborts the transaction,
/// but MySQL and SQLite undo only the failed statement (duplicate key
/// 1062, `SQLITE_BUSY`). If the closure ignores that error and returns
/// `Ok`, the other writes commit. A MySQL lock-wait timeout (1205) always
/// aborts the block.
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
    run(pool, End::Commit, f).await
}

/// How a block ends when its closure returns `Ok`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Commit,
    /// `test_db::with_rollback`: undo the writes, keep the value.
    Rollback,
}

/// An [`atomic`] block that always rolls back, so a nested `atomic` on
/// `pool` runs in a savepoint of it (#1761).
pub(crate) async fn rolled_back<F, T>(pool: &Pool, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    run(pool, End::Rollback, f).await
}

async fn run<F, T>(pool: &Pool, end: End, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let id = PoolId(pool.clone());
    let current = BLOCK.try_with(Arc::clone).ok();
    let same_pool =
        std::iter::successors(current.clone(), |b| b.enclosing.clone()).find(|b| b.pool.same(&id));
    match same_pool {
        Some(parent) => nested(parent, current, end, f).await,
        None => outermost(pool, id, current, end, f).await,
    }
}

async fn outermost<F, T>(
    pool: &Pool,
    id: PoolId,
    enclosing: Option<Arc<Block>>,
    end: End,
    f: F,
) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let fatal = Arc::new(AtomicBool::new(false));
    let mut tx = transaction_pool(pool).await?;
    let hook = Arc::clone(&fatal);
    tx.on_sqlite_rollback(move || hook.store(true, Ordering::SeqCst))
        .await?;
    let state = Arc::new(tokio::sync::Mutex::new(TxState {
        tx: Some(tx),
        depth: 0,
        poisoned: false,
    }));
    // The `PoolTx` never moves inside `state`, so its address identifies it.
    let tx_key = state
        .try_lock()
        .ok()
        .and_then(|g| g.tx.as_ref().map(|t| std::ptr::from_ref(t) as usize))
        .unwrap_or(0);
    let slot = Arc::new(Slot {
        state,
        cancelled: AtomicUsize::new(0),
        guards: AtomicUsize::new(0),
        claims: AtomicUsize::new(0),
        fatal,
        tx_key,
    });
    let block = Arc::new(Block {
        pool: id,
        slot: Arc::clone(&slot),
        depth: 0,
        enclosing,
        callbacks: Mutex::new(Vec::new()),
        committed: AtomicBool::new(false),
        warned: Mutex::new(BTreeSet::new()),
    });
    let handle = AtomicTx {
        block: Arc::clone(&block),
    };
    let res = BLOCK
        .scope(Arc::clone(&block), async { f(&handle).await })
        .await;
    // Waits for any background savepoint work, then rolls back a
    // cancelled nested block before deciding.
    let mut st = slot.state.lock().await;
    let settled = match st.settle(&slot).await {
        Ok(()) if st.depth != 0 => Err(ExecError::AtomicAborted),
        // PG turns a COMMIT after a failed statement into a silent ROLLBACK.
        Ok(()) if res.is_ok() && end == End::Commit => st.check_open(&slot).await,
        other => other,
    };
    let mut tx = st.tx.take().expect("atomic transaction is open");
    drop(st);
    tx.clear_sqlite_rollback().await;
    match res.and_then(|v| settled.map(|()| v)) {
        Ok(v) if end == End::Rollback => {
            tx.rollback().await?;
            Ok(v)
        }
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
    end: End,
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
    let depth = parent_depth + 1;
    let opening = Arc::new(Opening::default());
    // Armed before the SAVEPOINT is sent: a drop from here on is a cancel.
    let mut open = OpenSavepoint {
        slot: Arc::clone(&slot),
        depth,
        opening: Arc::clone(&opening),
        ending: false,
    };
    let (task_slot, task_opening) = (Arc::clone(&slot), Arc::clone(&opening));
    let claim = Claim::new(&slot);
    let opened = in_background(async move {
        let mut g = guard;
        // Only the innermost open block may open a child.
        if g.depth != parent_depth {
            return Err(ExecError::NestedAtomic);
        }
        g.savepoint("SAVEPOINT", depth).await?;
        g.depth = depth;
        task_opening.opened.store(true, Ordering::SeqCst);
        if task_opening.abandoned.load(Ordering::SeqCst) {
            mark_cancelled(&task_slot, depth);
        }
        Ok(())
    })
    .await;
    drop(claim);
    if let Err(e) = opened {
        open.ending = true;
        return Err(e);
    }
    let block = Arc::new(Block {
        pool: parent.pool.clone(),
        slot: Arc::clone(&slot),
        depth,
        enclosing,
        callbacks: Mutex::new(Vec::new()),
        committed: AtomicBool::new(false),
        warned: Mutex::new(BTreeSet::new()),
    });
    let handle = AtomicTx {
        block: Arc::clone(&block),
    };
    let res = BLOCK
        .scope(Arc::clone(&block), async { f(&handle).await })
        .await;
    let guard = match acquire(&slot).await {
        Ok(g) => g,
        // `open` is still armed: the next use rolls this savepoint back.
        Err(e) => return Err(res.err().unwrap_or(e)),
    };
    // From here the background task finishes the savepoint, callbacks
    // included, even if this future is dropped.
    open.ending = true;
    let release = res.is_ok() && end == End::Commit;
    let claim = Claim::new(&slot);
    let ended = in_background(async move {
        let mut g = guard;
        if g.depth != depth {
            g.poisoned = true;
            return Err(ExecError::AtomicAborted);
        }
        if release {
            g.savepoint("RELEASE SAVEPOINT", depth).await?;
            g.depth = depth - 1;
            let callbacks = block.take_callbacks();
            parent
                .callbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(callbacks);
            Ok(())
        } else {
            g.rollback_to(depth).await
        }
    })
    .await;
    drop(claim);
    // A block's own error wins over a failure to end its savepoint.
    let v = res?;
    ended?;
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

#[cfg(all(test, feature = "sqlite", feature = "runtime"))]
mod tests {
    use super::*;

    /// A `&Pool` call inside a block on that pool warns once per block (#1460).
    #[tokio::test]
    async fn pool_call_inside_block_warns_once() {
        let out = crate::testkit::CaptureWriter::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        let _sub = tracing::subscriber::set_default(sub);
        let pool = Pool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(2)
                .connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        crate::sql::raw_execute_pool(&pool, "SELECT 1", Vec::new())
            .await
            .unwrap();
        assert_eq!(out.contents(), "", "outside a block");
        for _ in 0..2 {
            let p = pool.clone();
            atomic(&pool, move |_tx| {
                Box::pin(async move {
                    for _ in 0..2 {
                        crate::sql::raw_execute_pool(&p, "SELECT 1", Vec::new()).await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
        }
        let logs = out.contents();
        assert_eq!(logs.matches("raw_execute_pool").count(), 2, "{logs}");
    }
}
