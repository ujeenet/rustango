//! `atomic()` + `on_commit()` — a closure-scoped transaction and its
//! after-commit hooks, rolled into one helper. Issue #44.
//!
//! The outermost block owns the transaction; a nested block on the same
//! pool runs in a savepoint on it (#1666). Each block's future carries its
//! own context in a task-local, so `join!`ed blocks cannot see each other's.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::OwnedMutexGuard;

use super::tx::TxStatus;
use super::{transaction_pool, write_transaction, ExecError, PoolTx};
use crate::sql::{Pool, PoolId};

type Callback = Box<dyn FnOnce() + Send>;

/// The transaction of one outermost block.
struct Slot {
    state: Arc<tokio::sync::Mutex<TxState>>,
    /// Shallowest savepoint whose block was dropped mid-flight (0 = none).
    cancelled: AtomicUsize,
    /// Earliest [`RetryPoint`] dropped before its end: its number and depth.
    cancelled_point: Mutex<Option<(usize, usize)>>,
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
    /// The server ended the transaction without a failed statement.
    ended: bool,
    /// [`RetryPoint`]s opened so far, so each gets its own name.
    retry_points: usize,
    /// Open [`RetryPoint`]s: number and depth.
    points: Vec<(usize, usize)>,
}

impl TxState {
    fn tx(&mut self) -> &mut PoolTx<'static> {
        self.tx.as_mut().expect("atomic transaction is open")
    }

    async fn savepoint(&mut self, verb: &str, depth: usize) -> Result<(), ExecError> {
        self.named_savepoint(verb, &format!("rustango_sp_{depth}"))
            .await
    }

    async fn named_savepoint(&mut self, verb: &str, name: &str) -> Result<(), ExecError> {
        let tx = self.tx();
        let name = tx.dialect().quote_ident(name);
        let r = tx.execute_unprepared(&format!("{verb} {name}")).await;
        if r.is_err() {
            self.poisoned = true;
        }
        Ok(r?)
    }

    async fn rollback_to(&mut self, depth: usize) -> Result<(), ExecError> {
        self.savepoint("ROLLBACK TO SAVEPOINT", depth).await?;
        self.savepoint("RELEASE SAVEPOINT", depth).await?;
        self.left(depth);
        Ok(())
    }

    /// Savepoint `depth` is gone: the retry points inside it went with it.
    fn left(&mut self, depth: usize) {
        self.depth = depth - 1;
        self.points.retain(|&(_, d)| d < depth);
    }

    /// Retry point `n` is gone, and every point opened after it.
    fn point_gone(&mut self, n: usize) {
        self.points.retain(|&(m, _)| m < n);
    }

    /// No savepoint could undo a failed statement now: PG aborts the block.
    fn unguarded(&self) -> bool {
        self.depth == 0 && self.points.is_empty()
    }

    /// Poison the transaction if the server already ended it.
    async fn check_open(&mut self, slot: &Slot) -> Result<(), ExecError> {
        let status = self.tx().status().await;
        if status == TxStatus::Open {
            return Ok(());
        }
        self.poisoned = true;
        // No failed statement seen: the server ended it on its own, e.g.
        // a MySQL DDL implicit commit, so some writes may be committed.
        if status == TxStatus::Ended && !slot.fatal.load(Ordering::SeqCst) {
            self.ended = true;
            return Err(ExecError::AtomicEndedEarly);
        }
        Err(ExecError::AtomicAborted)
    }

    /// Refuse a poisoned transaction; roll back a cancelled block first.
    async fn settle(&mut self, slot: &Slot) -> Result<(), ExecError> {
        if self.ended {
            return Err(ExecError::AtomicEndedEarly);
        }
        if self.poisoned || slot.fatal.load(Ordering::SeqCst) {
            return Err(ExecError::AtomicAborted);
        }
        let d = slot.cancelled.swap(0, Ordering::SeqCst);
        if d != 0 && d <= self.depth {
            self.rollback_to(d).await?;
        }
        let point = {
            let mut cp = slot
                .cancelled_point
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match *cp {
                // Rolled back or released with its enclosing savepoint.
                Some((_, depth)) if depth > self.depth => {
                    *cp = None;
                    None
                }
                Some((_, depth)) if depth == self.depth => cp.take(),
                // A savepoint opened after it is still open: wait for it.
                _ => None,
            }
        };
        if let Some((n, _)) = point {
            let name = retry_point_name(n);
            self.named_savepoint("ROLLBACK TO SAVEPOINT", &name).await?;
            self.named_savepoint("RELEASE SAVEPOINT", &name).await?;
            self.point_gone(n);
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
        && slot
            .cancelled_point
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
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

impl Gate {
    /// No transaction to guard: a pool or a held connection.
    pub(crate) const OPEN: Self = Self(None);
}

/// Look up the `atomic` transaction `tx` belongs to, and refuse to run on it
/// once the server ended it. Called by every `_tx` statement helper.
pub(crate) fn gate(tx: &PoolTx<'_>) -> Result<Gate, ExecError> {
    let key = std::ptr::from_ref::<PoolTx<'_>>(tx) as usize;
    let slot = std::iter::successors(current(), |b| b.enclosing.clone())
        .map(|b| Arc::clone(&b.slot))
        .find(|s| s.tx_key == key);
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

/// MySQL's lock-wait timeout (1205).
#[allow(unused_variables)]
fn is_lock_wait_timeout<T>(r: &Result<T, ExecError>) -> bool {
    #[cfg(feature = "mysql")]
    if let Err(ExecError::Driver(sqlx::Error::Database(db))) = r {
        return db
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .is_some_and(|m| m.number() == 1205);
    }
    false
}

/// MySQL rolls the whole transaction back on a deadlock (1213) and may on a
/// lock-wait timeout (1205); a lost connection ends it everywhere. A PG
/// FATAL or PANIC (e.g. 57P01, terminated backend) ends the session.
fn ends_transaction(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => true,
        #[allow(unused_variables)]
        sqlx::Error::Database(db) => {
            #[cfg(feature = "mysql")]
            if db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .is_some_and(|m| matches!(m.number(), 1213 | 1205))
            {
                return true;
            }
            #[cfg(feature = "postgres")]
            if db
                .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
                .is_some_and(|p| {
                    use sqlx::postgres::PgSeverity;
                    matches!(p.severity(), PgSeverity::Fatal | PgSeverity::Panic)
                })
            {
                return true;
            }
            false
        }
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
    /// The innermost block around the running future; `None` in [`detached`].
    static BLOCK: Option<Arc<Block>>;
}

/// The innermost block around the running future.
fn current() -> Option<Arc<Block>> {
    BLOCK.try_with(Clone::clone).ok().flatten()
}

/// Run `fut` outside any [`atomic`] block, so its pool statements take
/// their own connection and commit on their own.
#[cfg(any(feature = "cache", feature = "tenancy"))]
pub(crate) async fn detached<F: Future>(fut: F) -> F::Output {
    BLOCK.scope(None, fut).await
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

/// The innermost [`atomic`] block on `pool` around the running task.
fn block_on(pool: &Pool) -> Option<Arc<Block>> {
    let id = PoolId::of(pool);
    std::iter::successors(current(), |b| b.enclosing.clone()).find(|b| b.pool.same(&id))
}

/// Whether the running task is inside an [`atomic`] block on `pool`.
pub(crate) fn in_block(pool: &Pool) -> bool {
    block_on(pool).is_some()
}

/// How a statement joins a block.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stmt {
    /// A read or raw SQL: one statement, no savepoint. On PG its failure
    /// aborts the block.
    Plain,
    /// An ORM write: in a savepoint, so a failed one leaves the block usable.
    Write,
}

/// One statement's share of the open block on its pool. Holds the block's
/// lock, so a second statement or a [`TxGuard`] meanwhile is refused. A
/// write's savepoint, dropped unreleased, rolls back on the next use.
pub(crate) struct Joined {
    guard: OwnedMutexGuard<TxState>,
    slot: Arc<Slot>,
    phase: Phase,
}

/// Where a joined write's savepoint is, so a drop at any await leaves the
/// block's state right without a background task.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// A plain statement, or a write done with its savepoint.
    Done,
    /// SAVEPOINT sent: if it landed, the orphan nests harmlessly and goes
    /// with the next release or the commit; nothing to undo yet.
    Opening(usize),
    /// The savepoint is open: a drop rolls back to it on the next use.
    Open(usize),
    /// RELEASE sent: the write is kept either way.
    Releasing(usize),
}

/// `stmt`'s share of the [`atomic`] block on `pool`; `None` outside one.
/// Refused with [`ExecError::NestedAtomic`] while the block's guard is
/// held or another statement runs on it.
pub(crate) async fn join(pool: &Pool, stmt: Stmt) -> Option<Result<Joined, ExecError>> {
    let block = block_on(pool)?;
    Some(Joined::open(&block, stmt).await)
}

impl Joined {
    async fn open(block: &Block, stmt: Stmt) -> Result<Self, ExecError> {
        let slot = Arc::clone(&block.slot);
        if slot.fatal.load(Ordering::SeqCst) {
            return Err(ExecError::AtomicAborted);
        }
        let guard = acquire(&slot).await?;
        if guard.depth != block.depth {
            return Err(ExecError::NestedAtomic);
        }
        slot.guards.fetch_add(1, Ordering::SeqCst);
        let mut j = Self {
            guard,
            slot,
            phase: Phase::Done,
        };
        if stmt == Stmt::Write {
            let depth = block.depth + 1;
            j.phase = Phase::Opening(depth);
            j.guard.savepoint("SAVEPOINT", depth).await?;
            j.guard.depth = depth;
            j.phase = Phase::Open(depth);
        }
        Ok(j)
    }

    /// End the statement with its result `r`: record an error that ended the
    /// transaction, else release the write's savepoint. A failed write's
    /// savepoint rolls back on the block's next use.
    pub(crate) async fn finish<T>(mut self, r: Result<T, ExecError>) -> Result<T, ExecError> {
        let r = Gate(Some(Arc::clone(&self.slot))).check(r);
        // A lock-wait timeout (MariaDB reports a `NOWAIT` miss as one too)
        // undoes only the statement unless the server rolls back on it.
        if is_lock_wait_timeout(&r) && self.guard.tx().status().await == TxStatus::Open {
            self.slot.fatal.store(false, Ordering::SeqCst);
        }
        if let Err(ExecError::Driver(sqlx::Error::Database(_))) = &r {
            // No savepoint to undo it: PG has aborted the whole block.
            if self.guard.unguarded()
                && self
                    .guard
                    .tx
                    .as_ref()
                    .is_some_and(|t| t.dialect().name() == "postgres")
            {
                self.slot.fatal.store(true, Ordering::SeqCst);
            }
        }
        let v = r?;
        self.release().await?;
        Ok(v)
    }

    /// Keep the statement's writes: release its savepoint.
    pub(crate) async fn release(mut self) -> Result<(), ExecError> {
        let Phase::Open(depth) = self.phase else {
            return Ok(());
        };
        self.phase = Phase::Releasing(depth);
        self.guard.savepoint("RELEASE SAVEPOINT", depth).await?;
        self.guard.left(depth);
        self.phase = Phase::Done;
        Ok(())
    }
}

impl Drop for Joined {
    fn drop(&mut self) {
        match self.phase {
            Phase::Open(depth) => mark_cancelled(&self.slot, depth),
            // Kept whether or not the RELEASE ran; a failed one poisoned the block.
            Phase::Releasing(depth) => self.guard.left(depth),
            Phase::Done | Phase::Opening(_) => {}
        }
        self.slot.guards.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::ops::Deref for Joined {
    type Target = PoolTx<'static>;
    fn deref(&self) -> &PoolTx<'static> {
        self.guard.tx.as_ref().expect("atomic transaction is open")
    }
}

impl std::ops::DerefMut for Joined {
    fn deref_mut(&mut self) -> &mut PoolTx<'static> {
        self.guard.tx()
    }
}

/// Refuse `op` inside an [`atomic`] block on `pool`.
pub(crate) fn refuse_in_block(
    pool: &Pool,
    op: &'static str,
    reason: &'static str,
) -> Result<(), ExecError> {
    if in_block(pool) {
        return Err(ExecError::InAtomic { op, reason });
    }
    Ok(())
}

/// A savepoint that undoes one step of the block on `pool` if it fails, so
/// a PostgreSQL error inside it does not abort the block. Unlike
/// [`JoinedWrite`] it holds no lock and opens no depth: the step may lock
/// the block's guard or join writes to it. `None` outside a block, or while
/// a guard or nested block is live there: the step then runs as is.
pub(crate) async fn retry_point(pool: &Pool) -> Result<Option<RetryPoint>, ExecError> {
    match block_on(pool) {
        Some(block) => RetryPoint::open(&block).await,
        None => Ok(None),
    }
}

/// Run `step` in a savepoint of the block on `pool`, if any, for a caller
/// that catches its error and goes on: a failure then undoes only the step,
/// so PG does not abort the block. The step's own error wins.
pub(crate) async fn caught<T>(
    pool: &Pool,
    step: impl Future<Output = Result<T, ExecError>>,
) -> Result<T, ExecError> {
    let Some(point) = retry_point(pool).await? else {
        return step.await;
    };
    let r = step.await;
    let ended = point.end(r.is_ok()).await;
    r.and_then(|v| ended.map(|()| v))
}

fn retry_point_name(n: usize) -> String {
    format!("rustango_rp_{n}")
}

/// An open [`retry_point`]; finish it with [`RetryPoint::end`]. Dropped
/// unfinished, it is rolled back on the block's next use.
pub(crate) struct RetryPoint {
    slot: Arc<Slot>,
    depth: usize,
    n: usize,
    done: bool,
}

impl RetryPoint {
    async fn open(block: &Block) -> Result<Option<Self>, ExecError> {
        let slot = Arc::clone(&block.slot);
        let mut guard = match acquire(&slot).await {
            Ok(g) => g,
            Err(ExecError::NestedAtomic) => return Ok(None),
            Err(e) => return Err(e),
        };
        if guard.depth != block.depth {
            return Ok(None);
        }
        // Its own name: a nested retry point must not replace it (MySQL).
        guard.retry_points += 1;
        let n = guard.retry_points;
        // Finished even if the caller is dropped; an orphan is released
        // with its enclosing savepoint or the commit.
        let claim = Claim::new(&slot);
        let depth = block.depth;
        let opened = in_background(async move {
            let mut g = guard;
            g.named_savepoint("SAVEPOINT", &retry_point_name(n)).await?;
            g.points.push((n, depth));
            Ok(())
        })
        .await;
        drop(claim);
        opened?;
        Ok(Some(Self {
            slot,
            depth: block.depth,
            n,
            done: false,
        }))
    }

    /// Keep the step's writes, or roll them back.
    pub(crate) async fn end(mut self, keep: bool) -> Result<(), ExecError> {
        // Settles the step's cancelled savepoints first.
        let guard = match acquire(&self.slot).await {
            Ok(g) => g,
            // A guard is live: a kept point stays open and commits with the
            // block; a rollback is left to the next settle (`Drop`).
            Err(ExecError::NestedAtomic) => {
                self.done = keep;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if guard.depth != self.depth {
            self.done = keep;
            return Ok(());
        }
        let (n, name) = (self.n, retry_point_name(self.n));
        // Finished in the background even if the caller is dropped, so a
        // later settle never rolls back to a released point.
        self.done = true;
        let _claim = Claim::new(&self.slot);
        in_background(async move {
            let mut g = guard;
            if !keep {
                g.named_savepoint("ROLLBACK TO SAVEPOINT", &name).await?;
            }
            g.named_savepoint("RELEASE SAVEPOINT", &name).await?;
            g.point_gone(n);
            Ok(())
        })
        .await
    }
}

impl Drop for RetryPoint {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut cp = self
            .slot
            .cancelled_point
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Rolling back to the earliest point undoes the later ones too.
        if cp.is_none_or(|(n, _)| self.n < n) {
            *cp = Some((self.n, self.depth));
        }
    }
}

/// Whether the running task is inside an [`atomic`] block whose commit
/// is still ahead, on any pool.
#[cfg(feature = "jobs")]
pub(crate) fn in_any_block() -> bool {
    current().is_some_and(|b| !b.committed.load(Ordering::SeqCst))
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
/// statement and pass `&mut *guard` to the `_tx` helpers, or match
/// [`PoolTx::conn`] for raw sqlx.
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
/// **ORM calls on the pool** (reads, writes, raw SQL and DDL given the same
/// `&Pool`) run on the block's transaction, so they see its uncommitted rows
/// and its rollback undoes them. ORM writes run in a savepoint, so a failed
/// one (an ignored duplicate) leaves the block usable. Reads and raw SQL are
/// one plain statement: on PG a failed one (a caught `nowait` read) aborts the
/// block, and later statements and `atomic` return [`ExecError::AtomicAborted`];
/// wrap such a step in a nested `atomic` to keep going. One statement at a time: with a
/// [`TxGuard`] held, or two statements at once (`join!`), they return
/// [`ExecError::NestedAtomic`]. MySQL and SQLite reads see one snapshot for
/// the whole block. On MySQL, [`run_ddl_idempotent`](crate::sql::run_ddl_idempotent)
/// refuses DDL with an `sqlx::Error::Protocol`; raw DDL commits the block.
///
/// **Separate connections** ([`Pool::acquire`], `Tenant::conn`, the cache,
/// the job claim, tenancy registry changes) run outside the block's transaction: they do not see its
/// writes and can deadlock on locks it holds. On SQLite every block holds the
/// write lock from `BEGIN`, so their writes wait `busy_timeout` and then fail
/// with `SQLITE_BUSY` (file databases too); in-memory ones wait forever.
///
/// **Raw SQL** that ends the transaction (`COMMIT`, `ROLLBACK`, an implicit
/// commit) is found at the outermost commit, which then returns
/// [`ExecError::AtomicEndedEarly`]; statements after it ran in autocommit.
///
/// **Server-ended transactions:** after a statement error that ends the
/// whole transaction (MySQL deadlock 1213 / timeout 1205, SQLite automatic
/// rollback, a PG FATAL such as a terminated backend, a lost connection)
/// every later ORM statement in the block
/// returns [`ExecError::AtomicAborted`], and so does `atomic`. Before the
/// outermost COMMIT the block also checks the transaction is still open
/// (one round trip on PG and SQLite, two on MySQL): a PG error the closure ignored
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
/// `Ok`, the other writes commit. A MySQL lock-wait timeout (1205, also
/// MariaDB's `NOWAIT` miss) aborts the block when the server rolled the
/// transaction back; ORM calls on the pool check, statements on a guard
/// always abort.
///
/// **SQLite** begins the outermost block with `BEGIN IMMEDIATE`, so a
/// read-then-write block waits for the write lock instead of failing with
/// `SQLITE_BUSY`; other writers wait for the block.
///
/// **Costs:** a read or raw statement costs one round trip; an ORM write
/// three (its savepoint). Past 64 subtransactions in one transaction
/// PostgreSQL spills its subtransaction cache, and every ORM write in a block
/// is one, so keep blocks with many writes or nested blocks short. On
/// MySQL a deadlock inside a nested block rolls back the whole outer
/// transaction.
///
/// **Spawned tasks** do not inherit the block: `atomic` inside
/// `tokio::spawn` opens a separate transaction (a real, independent
/// commit), and `on_commit` there panics.
///
/// **Callbacks fire in registration order**, serially, after the
/// `COMMIT` returns OK. A panicking callback is logged and the rest still run.
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
    atomic_with(pool, AtomicOptions::new(), f).await
}

/// [`atomic`] with an isolation level or transaction settings, applied when
/// the outermost block begins:
///
/// ```no_run
/// # async fn demo(pool: &rustango::sql::Pool) -> Result<(), rustango::sql::ExecError> {
/// use rustango::sql::{atomic_with, AtomicOptions, Isolation, TxSetting};
/// let opts = AtomicOptions::new()
///     .isolation(Isolation::Serializable)
///     .setting(TxSetting::new("app.account_id", "42")?);
/// atomic_with(pool, opts, |_tx| Box::pin(async move { Ok(()) })).await
/// # }
/// ```
///
/// PG begins `BEGIN ISOLATION LEVEL …` and sets each setting with
/// `set_config(name, value, true)`, gone at the end of the block. MySQL sets
/// the level for this transaction only. SQLite is always serializable, so
/// any level is accepted. Settings are PG-only. A block with neither sends
/// what [`atomic`] sends.
///
/// # Errors
/// As [`atomic`]; settings on MySQL or SQLite are
/// [`SqlError::OperatorNotSupportedInDialect`](crate::sql::SqlError::OperatorNotSupportedInDialect),
/// and options on a nested block (it cannot change them) [`ExecError::InAtomic`].
pub async fn atomic_with<F, T>(pool: &Pool, opts: AtomicOptions, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    run(pool, End::Commit, opts, f).await
}

/// Whether the running task is inside an [`atomic`] block on `pool`, so
/// ORM calls given `pool` run on its transaction. Also `true` for a block
/// that failed or ended early: calls there return
/// [`ExecError::AtomicAborted`] or [`ExecError::AtomicEndedEarly`].
#[must_use]
pub fn in_atomic(pool: &Pool) -> bool {
    in_block(pool)
}

/// How [`atomic_with`] begins a block. Built with its methods; the default
/// is what [`atomic`] does.
#[derive(Debug, Clone)]
#[must_use]
pub struct AtomicOptions {
    isolation: Option<Isolation>,
    settings: Vec<TxSetting>,
    begin: Begin,
}

impl Default for AtomicOptions {
    fn default() -> Self {
        Self {
            isolation: None,
            settings: Vec::new(),
            begin: Begin::Immediate,
        }
    }
}

impl AtomicOptions {
    /// The options [`atomic`] uses.
    pub fn new() -> Self {
        Self::default()
    }

    /// The outermost transaction's isolation level.
    pub fn isolation(mut self, level: Isolation) -> Self {
        self.isolation = Some(level);
        self
    }

    /// A PG setting for the block's transaction only.
    pub fn setting(mut self, setting: TxSetting) -> Self {
        self.settings.push(setting);
        self
    }

    fn changes_the_begin(&self) -> bool {
        self.isolation.is_some() || !self.settings.is_empty()
    }
}

/// A transaction isolation level for [`AtomicOptions::isolation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Isolation {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl Isolation {
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn sql(self) -> &'static str {
        match self {
            Self::ReadCommitted => "READ COMMITTED",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }
}

/// A PG setting set for one transaction, such as `app.account_id` for row
/// security policies. The name must be dotted (`prefix.name`, more dots
/// allowed), every part a plain identifier: that excludes the server's
/// undotted settings such as `search_path` and `role`, not extension ones.
#[derive(Debug, Clone)]
pub struct TxSetting {
    name: String,
    value: String,
}

impl TxSetting {
    /// `name = value` for the block's transaction. The value is bound.
    ///
    /// # Errors
    /// [`ExecError::InvalidTxSetting`] unless `name` is dotted with every
    /// part a plain identifier.
    pub fn new(name: &str, value: impl Into<String>) -> Result<Self, ExecError> {
        let plain = name.contains('.') && name.split('.').all(crate::core::is_plain_identifier);
        if !plain {
            return Err(ExecError::InvalidTxSetting {
                name: name.to_owned(),
            });
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.into(),
        })
    }
}

/// [`atomic`], which now begins SQLite blocks with `BEGIN IMMEDIATE` too.
///
/// # Errors
/// As [`atomic`].
#[deprecated(
    since = "0.67.0",
    note = "use `atomic`: it begins SQLite blocks with `BEGIN IMMEDIATE` now"
)]
pub async fn atomic_immediate<F, T>(pool: &Pool, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    atomic_with(pool, AtomicOptions::new(), f).await
}

/// How a top-level SQLite block begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Begin {
    Deferred,
    Immediate,
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
    let opts = AtomicOptions {
        begin: Begin::Deferred,
        ..AtomicOptions::new()
    };
    run(pool, End::Rollback, opts, f).await
}

async fn run<F, T>(pool: &Pool, end: End, opts: AtomicOptions, f: F) -> Result<T, ExecError>
where
    F: for<'tx> FnOnce(
        &'tx AtomicTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, ExecError>> + Send + 'tx>,
    >,
{
    let id = PoolId::of(pool);
    let enclosing = current();
    let same_pool = std::iter::successors(enclosing.clone(), |b| b.enclosing.clone())
        .find(|b| b.pool.same(&id));
    match same_pool {
        // PG keeps a nested `set_config` past RELEASE; levels are refused.
        Some(_) if opts.changes_the_begin() => Err(ExecError::InAtomic {
            op: "an isolation level or setting",
            reason: "it applies only when the outermost block begins",
        }),
        Some(parent) => nested(parent, enclosing, end, f).await,
        None => outermost(pool, id, enclosing, end, &opts, f).await,
    }
}

/// Begin the outermost block's transaction as `opts` say. Without a level or
/// settings it sends what it always did.
async fn begin_block(
    pool: &Pool,
    opts: &AtomicOptions,
    top: bool,
) -> Result<PoolTx<'static>, ExecError> {
    if !opts.settings.is_empty() && pool.dialect().name() != "postgres" {
        return Err(ExecError::Sql(
            crate::sql::SqlError::OperatorNotSupportedInDialect {
                op: "set_config",
                dialect: pool.dialect().name(),
            },
        ));
    }
    let mut tx = match (pool, opts.isolation) {
        #[cfg(feature = "postgres")]
        (Pool::Postgres(pg), Some(level)) => PoolTx::new(
            super::TxConn::Postgres(
                pg.begin_with(format!("BEGIN ISOLATION LEVEL {}", level.sql()))
                    .await?,
            ),
            PoolId::of(pool),
        ),
        // One statement, so a cancel cannot leave the level for the next borrower.
        #[cfg(feature = "mysql")]
        (Pool::Mysql(my), Some(level)) => PoolTx::new(
            super::TxConn::Mysql(
                my.begin_with(format!(
                    "SET TRANSACTION ISOLATION LEVEL {}; START TRANSACTION",
                    level.sql()
                ))
                .await?,
            ),
            PoolId::of(pool),
        ),
        // SQLite is always serializable.
        _ if opts.begin == Begin::Immediate && top => write_transaction(pool).await?,
        _ => transaction_pool(pool).await?,
    };
    for s in &opts.settings {
        let d = tx.dialect();
        let sql = format!(
            "SELECT set_config({}, {}, true)",
            d.placeholder(1),
            d.placeholder(2)
        );
        let binds = vec![s.name.clone().into(), s.value.clone().into()];
        super::raw_execute_tx(&mut tx, &sql, binds).await?;
    }
    Ok(tx)
}

async fn outermost<F, T>(
    pool: &Pool,
    id: PoolId,
    enclosing: Option<Arc<Block>>,
    end: End,
    opts: &AtomicOptions,
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
    let mut tx = begin_block(pool, opts, enclosing.is_none()).await?;
    let hook = Arc::clone(&fatal);
    tx.on_sqlite_rollback(move || hook.store(true, Ordering::SeqCst))
        .await?;
    let state = Arc::new(tokio::sync::Mutex::new(TxState {
        tx: Some(tx),
        depth: 0,
        poisoned: false,
        ended: false,
        retry_points: 0,
        points: Vec::new(),
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
        cancelled_point: Mutex::new(None),
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
    });
    let handle = AtomicTx {
        block: Arc::clone(&block),
    };
    let res = BLOCK
        .scope(Some(Arc::clone(&block)), async { f(&handle).await })
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
            BLOCK.sync_scope(Some(block), || {
                for cb in callbacks {
                    // The data is committed: one panicking hook must not skip the rest.
                    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cb));
                    if let Err(panic) = run {
                        tracing::error!(
                            target: "rustango::error",
                            panic = crate::panic_guard::panic_message(&*panic),
                            "on_commit callback panicked"
                        );
                    }
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
    });
    let handle = AtomicTx {
        block: Arc::clone(&block),
    };
    let res = BLOCK
        .scope(Some(Arc::clone(&block)), async { f(&handle).await })
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
            g.left(depth);
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
    let block = current().unwrap_or_else(|| {
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
    std::iter::successors(current(), |b| b.enclosing.clone())
        .map(|b| {
            b.callbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
        })
        .sum()
}
