//! Background job queue — async work outside the request lifecycle.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::jobs::{Job, JobQueue, InMemoryJobQueue, JobError};
//! use serde::{Serialize, Deserialize};
//! use std::sync::Arc;
//!
//! #[derive(Serialize, Deserialize)]
//! struct SendWelcomeEmail { user_id: i64 }
//!
//! #[async_trait::async_trait]
//! impl Job for SendWelcomeEmail {
//!     const NAME: &'static str = "welcome_email";
//!
//!     async fn run(&self) -> Result<(), JobError> {
//!         // ... send the email
//!         Ok(())
//!     }
//! }
//!
//! // At startup:
//! let queue = Arc::new(InMemoryJobQueue::with_workers(4));
//! queue.register::<SendWelcomeEmail>().await;
//! queue.start().await;
//!
//! // From a handler:
//! queue.dispatch(&SendWelcomeEmail { user_id: 42 }).await?;
//!
//! // On shutdown:
//! queue.shutdown().await;
//! ```
//!
//! ## Backends
//!
//! | Backend | When to use |
//! |---|---|
//! | [`InMemoryJobQueue`] | One process: dev, tests, small apps. Jobs are lost on restart. |
//! | `pg::PgJobQueue` (feature `jobs-postgres`) | Many processes or replicas. Runs on PostgreSQL, MySQL 8+ or SQLite. |
//!
//! ## Retry policy
//!
//! A job that returns `Err(JobError::Retryable(_))` is retried with
//! growing backoff (1s, 2s, 4s, 8s, … up to 1024s). `max_attempts` counts **all**
//! runs, so the default of 5 means one run plus four retries.
//! `Err(JobError::Fatal(_))` goes straight to the dead-letter handler.
//!
//! [`InMemoryJobQueue`]: crate::jobs::InMemoryJobQueue

#[cfg(feature = "jobs-postgres")]
pub mod pg;

/// Alias for [`pg::PgJobQueue`]. The queue is not PG-only — the `Pg`
/// prefix is historical. Prefer this name in new code.
#[cfg(feature = "jobs-postgres")]
pub type DatabaseJobQueue = pg::PgJobQueue;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// Transient failure — worker will retry with exponential backoff.
    #[error("retryable: {0}")]
    Retryable(String),
    /// Permanent failure — goes straight to dead-letter, no retries.
    #[error("fatal: {0}")]
    Fatal(String),
    /// Internal error in the queue itself (serialization, registration, etc.).
    #[error("queue error: {0}")]
    Queue(String),
}

// ------------------------------------------------------------------ Job trait

/// One unit of background work.
///
/// `NAME` identifies the job kind for routing (the queue uses it to find
/// the registered handler). Two job structs can't share the same NAME.
#[async_trait::async_trait]
pub trait Job: Send + Sync + Sized + Serialize + DeserializeOwned + 'static {
    /// Stable identifier for this job kind. Routes payloads to handlers.
    const NAME: &'static str;

    /// Cap on **total** runs, not extra retries. The default of 5 is
    /// one run plus four retries; 3 gives two retries. 0 counts as 1.
    const MAX_ATTEMPTS: u32 = 5;

    /// Run the job. Return `Ok(())` on success, `Err(Retryable(_))` to
    /// retry with backoff, `Err(Fatal(_))` to dead-letter at once.
    ///
    /// **Which ambient context reaches here depends on the queue.**
    /// [`tokio::spawn`] inherits no `tokio::task_local!` state, so the
    /// queue must carry it — see [`crate::task_context::TaskContext`].
    ///
    /// Both queues carry the enqueuer's audit source and active
    /// timezone. `PgJobQueue` keeps them in `rustango_jobs.context`,
    /// which `PgJobQueue::ensure_table_pool` adds; without that column
    /// its jobs run as `System` in the default timezone.
    ///
    /// A source set by the tenant admin is recorded only on writes to
    /// its own tenant: write through `tenancy::with_tenant`.
    ///
    /// **Neither queue carries a session or a tenant.** Anything else a
    /// job needs travels in its payload.
    async fn run(&self) -> Result<(), JobError>;
}

// ------------------------------------------------------------------ Queue trait

/// Pluggable job-queue backend.
#[async_trait::async_trait]
pub trait JobQueue: Send + Sync + 'static {
    /// Register a job type. Must be called before `dispatch::<T>` or `start`.
    async fn register<T: Job>(&self);

    /// Enqueue a job for asynchronous execution.
    async fn dispatch<T: Job>(&self, payload: &T) -> Result<(), JobError>;

    /// Spawn worker tasks. Calling `start` twice is a no-op; `start`
    /// after [`Self::shutdown`] starts a fresh set of workers.
    async fn start(&self);

    /// Stop all workers: running jobs get the shutdown grace period
    /// ([`DEFAULT_SHUTDOWN_GRACE`]), then are aborted and handed back
    /// to the queue. Queued jobs stay queued for the next `start`.
    ///
    /// No signal reaches a running job, and an aborted one runs again
    /// from the start, so jobs are at-least-once: make them idempotent.
    async fn shutdown(&self);

    /// Number of jobs currently queued (waiting to be picked up).
    async fn pending_count(&self) -> usize;
}

// ------------------------------------------------------------------ Lifecycle

/// How long `shutdown` lets running jobs finish before aborting them.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The workers of one `start()`. A queue holds `None` while stopped, so
/// each restart gets a fresh stop signal (#1677).
pub(crate) struct Running {
    workers: Vec<JoinHandle<()>>,
    stop: watch::Sender<bool>,
}

/// A worker's view of its run's stop signal.
#[derive(Clone)]
pub(crate) struct StopSignal(watch::Receiver<bool>);

impl StopSignal {
    #[cfg_attr(not(feature = "jobs-postgres"), allow(dead_code))]
    /// Also true once the queue is dropped without `shutdown`.
    pub(crate) fn is_set(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }

    /// Resolves once the run stops, or its queue is dropped.
    pub(crate) async fn wait(&mut self) {
        let _ = self.0.wait_for(|stop| *stop).await;
    }
}

impl Running {
    pub(crate) fn new() -> (Self, StopSignal) {
        let (stop, rx) = watch::channel(false);
        let run = Self {
            workers: Vec::new(),
            stop,
        };
        (run, StopSignal(rx))
    }

    pub(crate) fn push(&mut self, worker: JoinHandle<()>) {
        self.workers.push(worker);
    }

    /// Signal stop, wait up to `grace` for every worker, then abort the
    /// rest. Returns the indexes of the aborted workers.
    pub(crate) async fn stop(self, grace: Duration) -> Vec<usize> {
        let _ = self.stop.send(true);
        let deadline = tokio::time::Instant::now() + grace;
        let mut aborted = Vec::new();
        for (n, mut h) in self.workers.into_iter().enumerate() {
            if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
                h.abort();
                let _ = h.await;
                aborted.push(n);
            }
        }
        aborted
    }
}

// ------------------------------------------------------------------ JobEnvelope

#[derive(Debug, Clone)]
struct JobEnvelope {
    name: &'static str,
    payload: serde_json::Value,
    attempt: u32,
    max_attempts: u32,
    /// The caller's ambient context, captured at `dispatch`.
    ///
    /// Captured here rather than where the worker spawns, because
    /// workers are spawned once at `start()` — by then the caller is
    /// long gone and there is nothing to inherit. The context has to
    /// travel with the job.
    context: crate::task_context::TaskContext,
    /// A parked retry's due time; it survives a shutdown and restart.
    not_before: Option<tokio::time::Instant>,
}

// ------------------------------------------------------------------ Handler registry

/// Type-erased async handler. The queue stores one per registered Job::NAME.
type HandlerFn = Arc<
    dyn Fn(serde_json::Value) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send>>
        + Send
        + Sync,
>;

#[derive(Default)]
struct HandlerRegistry {
    handlers: HashMap<&'static str, (HandlerFn, u32)>, // (handler, max_attempts)
}

impl HandlerRegistry {
    fn register<T: Job>(&mut self) {
        let handler: HandlerFn = Arc::new(move |payload| {
            Box::pin(async move {
                let job: T =
                    serde_json::from_value(payload).map_err(|e| JobError::Queue(e.to_string()))?;
                // A panic is a failed run, not a dead worker (#1843).
                crate::panic_guard::catch_unwind(job.run())
                    .await
                    .unwrap_or_else(|panic| {
                        let msg = crate::panic_guard::panic_message(&*panic);
                        Err(JobError::Retryable(format!("job panicked: {msg}")))
                    })
            })
        });
        self.handlers
            .insert(T::NAME, (handler, max_attempts::<T>()));
    }

    fn lookup(&self, name: &str) -> Option<(HandlerFn, u32)> {
        self.handlers.get(name).cloned()
    }
}

// ------------------------------------------------------------------ DeadLetter

/// Callback invoked when a job exhausts retries or returns `Fatal`.
pub type DeadLetterFn =
    Arc<dyn Fn(JobDeadLetter) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct JobDeadLetter {
    pub name: &'static str,
    pub payload: serde_json::Value,
    pub attempts: u32,
    pub error: String,
}

/// Run a dead-letter callback; a panic in it is logged, not propagated.
pub(crate) async fn deliver_dead_letter(cb: DeadLetterFn, dl: JobDeadLetter) {
    let name = dl.name;
    if let Err(panic) = crate::panic_guard::catch_unwind(cb(dl)).await {
        let msg = crate::panic_guard::panic_message(&*panic);
        tracing::error!(job = name, panic = msg, "dead-letter callback panicked");
    }
}

/// [`Job::MAX_ATTEMPTS`] as the queues read it: 0 would never run (#2333).
pub(crate) fn max_attempts<T: Job>() -> u32 {
    T::MAX_ATTEMPTS.max(1)
}

/// Milliseconds to wait before the retry that follows `failed_attempt`:
/// 1s, 2s, 4s, 8s, … capped at 2^10 s.
///
/// `failed_attempt` is **0-based** — the index of the run that just
/// failed — so the first retry waits 1s. Shared by both backends so
/// they cannot drift apart.
pub(crate) fn retry_backoff_ms(failed_attempt: u32) -> u64 {
    1000u64.saturating_mul(1u64 << failed_attempt.min(10))
}

// ------------------------------------------------------------------ InMemoryJobQueue

/// In-process job queue built on tokio mpsc channels.
///
/// **Nothing is persisted.** Queued and in-flight jobs are lost when
/// the process restarts. For production or multi-process deploys use
/// `pg::PgJobQueue` (feature `jobs-postgres`).
pub struct InMemoryJobQueue {
    tx: mpsc::UnboundedSender<JobEnvelope>,
    /// Owned by the queue, not a run, so jobs outlive a `shutdown`.
    rx: Arc<Mutex<mpsc::UnboundedReceiver<JobEnvelope>>>,
    registry: Arc<Mutex<HandlerRegistry>>,
    run: Mutex<Option<InMemoryRun>>,
    worker_count: usize,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    /// Backoff timers; `shutdown` fires them at once and joins them.
    retries: Arc<std::sync::Mutex<JoinSet<()>>>,
    shutdown_grace: Duration,
}

/// The job each worker is running, so an aborted one can be re-queued.
type CurrentJob = Arc<std::sync::Mutex<Option<JobEnvelope>>>;

struct InMemoryRun {
    run: Running,
    current: Vec<CurrentJob>,
}

impl InMemoryJobQueue {
    /// Build a queue with `worker_count` worker tasks (call `start` to spawn them).
    #[must_use]
    pub fn with_workers(worker_count: usize) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            registry: Arc::new(Mutex::new(HandlerRegistry::default())),
            run: Mutex::new(None),
            worker_count,
            dead_letter: Arc::new(Mutex::new(None)),
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            retries: Arc::default(),
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// How long `shutdown` waits for running jobs before aborting them
    /// (default [`DEFAULT_SHUTDOWN_GRACE`]). An aborted job is re-queued.
    #[must_use]
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// Default: 4 workers.
    #[must_use]
    pub fn new() -> Self {
        Self::with_workers(4)
    }

    /// Set a callback invoked for jobs that exhaust retries or return Fatal.
    /// Use this to write dead-letter rows to a DB / Slack / Sentry.
    pub async fn on_dead_letter<F, Fut>(&self, callback: F)
    where
        F: Fn(JobDeadLetter) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let boxed: DeadLetterFn = Arc::new(move |dl| Box::pin(callback(dl)));
        *self.dead_letter.lock().await = Some(boxed);
    }
}

impl Default for InMemoryJobQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Build an [`InMemoryJobQueue`] from a loaded
/// [`crate::config::JobsSettings`]. Uses `s.concurrency`, or 4 workers
/// when it is unset.
///
/// ## Why only the in-memory backend?
///
/// [`JobQueue`] is not object-safe: `register<T>` and `dispatch<T>`
/// are generic, so `Arc<dyn JobQueue>` does not compile. There can be
/// no runtime backend picker with one shared return type. Wire any
/// other backend yourself:
///
/// ```ignore
/// let queue = match cfg.jobs.backend.as_deref() {
///     Some("pg") => Arc::new(rustango::jobs::pg::PgJobQueue::new(pool.clone())),
///     _ => rustango::jobs::inmemory_from_settings(&cfg.jobs),
/// };
/// ```
///
/// This function only reads `s.backend` to warn on a mismatch.
#[cfg(feature = "config")]
#[must_use]
pub fn inmemory_from_settings(s: &crate::config::JobsSettings) -> Arc<InMemoryJobQueue> {
    let workers = s.concurrency.map_or(4, |c| c as usize);
    if let Some(backend) = s.backend.as_deref() {
        if backend != "memory" {
            tracing::warn!(
                target: "rustango::jobs",
                backend = %backend,
                "jobs.backend = `{backend}` but inmemory_from_settings only builds InMemoryJobQueue. \
                 Wire the desired backend directly via Arc::new(...). See the docstring."
            );
        }
    }
    Arc::new(InMemoryJobQueue::with_workers(workers))
}

#[async_trait::async_trait]
impl JobQueue for InMemoryJobQueue {
    async fn register<T: Job>(&self) {
        self.registry.lock().await.register::<T>();
    }

    async fn dispatch<T: Job>(&self, payload: &T) -> Result<(), JobError> {
        let value = serde_json::to_value(payload).map_err(|e| JobError::Queue(e.to_string()))?;
        let envelope = JobEnvelope {
            name: T::NAME,
            payload: value,
            attempt: 0,
            max_attempts: max_attempts::<T>(),
            // Captured here, at the hand-off. A worker is spawned at
            // boot and has no caller to inherit from.
            context: crate::task_context::TaskContext::capture(),
            not_before: None,
        };
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The queue owns the receiver, so this only fails mid-drop.
        self.tx.send(envelope).map_err(|e| {
            self.pending
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            JobError::Queue(e.to_string())
        })?;
        Ok(())
    }

    async fn start(&self) {
        let mut slot = self.run.lock().await;
        if slot.is_some() {
            return; // already started
        }
        let (mut run, stop) = Running::new();
        let mut current = Vec::with_capacity(self.worker_count);
        for _ in 0..self.worker_count {
            let job = CurrentJob::default();
            current.push(job.clone());
            let worker = InMemoryWorker {
                rx: self.rx.clone(),
                registry: self.registry.clone(),
                dead_letter: self.dead_letter.clone(),
                pending: self.pending.clone(),
                tx: self.tx.clone(),
                retries: self.retries.clone(),
                stop: stop.clone(),
                current: job,
            };
            run.push(tokio::spawn(worker_loop(worker)));
        }
        *slot = Some(InMemoryRun { run, current });
    }

    async fn shutdown(&self) {
        let mut slot = self.run.lock().await;
        let Some(InMemoryRun { run, current }) = slot.take() else {
            return;
        };
        for n in run.stop(self.shutdown_grace).await {
            // Still counted in `pending`; back on the queue for the next
            // start, with the run spent as on the DB queue.
            if let Some(mut job) = current[n].lock().unwrap().take() {
                tracing::warn!(job = job.name, "job aborted at shutdown; re-queued");
                job.attempt += 1;
                let _ = self.tx.send(job);
            }
        }
        // Stop is set, so every parked retry re-queues now, due time kept.
        let mut timers = std::mem::take(&mut *self.retries.lock().unwrap());
        while timers.join_next().await.is_some() {}
    }

    async fn pending_count(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct InMemoryWorker {
    rx: Arc<Mutex<mpsc::UnboundedReceiver<JobEnvelope>>>,
    registry: Arc<Mutex<HandlerRegistry>>,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    tx: mpsc::UnboundedSender<JobEnvelope>,
    retries: Arc<std::sync::Mutex<JoinSet<()>>>,
    stop: StopSignal,
    current: CurrentJob,
}

async fn worker_loop(w: InMemoryWorker) {
    let InMemoryWorker {
        rx,
        registry,
        dead_letter,
        pending,
        tx,
        retries,
        mut stop,
        current,
    } = w;
    loop {
        // Stop wins over a ready job; `recv` is cancel-safe.
        let envelope = tokio::select! {
            biased;
            () = stop.wait() => return,
            e = async { rx.lock().await.recv().await } => match e {
                Some(e) => e,
                None => return, // channel closed
            },
        };

        // Before any await, so an abort from here on re-queues it.
        *current.lock().unwrap() = Some(envelope.clone());
        if let Some(until) = envelope
            .not_before
            .filter(|t| *t > tokio::time::Instant::now())
        {
            current.lock().unwrap().take();
            park(&retries, &tx, stop.clone(), envelope, until);
            continue;
        }
        if envelope.attempt >= envelope.max_attempts {
            // Aborted at shutdown on its last run: dead-letter, do not rerun.
            current.lock().unwrap().take();
            pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            let error = "attempts spent by runs aborted at shutdown".to_owned();
            let attempts = envelope.attempt;
            dead_letter_job(&dead_letter, envelope, attempts, error).await;
            continue;
        }

        let handler = {
            let reg = registry.lock().await;
            reg.lookup(envelope.name)
        };

        let Some((handler, _max_attempts)) = handler else {
            current.lock().unwrap().take();
            tracing::error!(job = envelope.name, "no handler registered");
            pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            continue;
        };

        let payload = envelope.payload.clone();
        // Run the handler inside the context the caller had at
        // `dispatch`. Without this the job sees none of it — an audit
        // row written here would record `system` and lose the actor.
        let result = envelope.context.clone().install(handler(payload)).await;
        current.lock().unwrap().take();

        match result {
            Ok(()) => {
                pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
            Err(JobError::Retryable(msg)) => {
                let next_attempt = envelope.attempt + 1;
                if next_attempt >= envelope.max_attempts {
                    // Count it done first: an abort in the callback must not leak `pending`.
                    pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    dead_letter_job(&dead_letter, envelope, next_attempt, msg).await;
                } else {
                    let backoff = Duration::from_millis(retry_backoff_ms(envelope.attempt));
                    let mut retry = envelope;
                    retry.attempt = next_attempt;
                    // Still counted in `pending` while parked.
                    let until = tokio::time::Instant::now() + backoff;
                    park(&retries, &tx, stop.clone(), retry, until);
                }
            }
            Err(e @ (JobError::Fatal(_) | JobError::Queue(_))) => {
                pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                let attempts = envelope.attempt + 1;
                dead_letter_job(&dead_letter, envelope, attempts, e.to_string()).await;
            }
        }
    }
}

/// Hold `job` until `until`, then queue it. A shutdown queues it at once
/// with `until` kept, so the next run parks it again (#1255).
fn park(
    retries: &std::sync::Mutex<JoinSet<()>>,
    tx: &mpsc::UnboundedSender<JobEnvelope>,
    mut stop: StopSignal,
    mut job: JobEnvelope,
    until: tokio::time::Instant,
) {
    job.not_before = Some(until);
    let tx = tx.clone();
    let mut timers = retries.lock().unwrap();
    while timers.try_join_next().is_some() {}
    timers.spawn(async move {
        tokio::select! {
            () = tokio::time::sleep_until(until) => {}
            () = stop.wait() => {}
        }
        let _ = tx.send(job);
    });
}

async fn dead_letter_job(
    dead_letter: &Mutex<Option<DeadLetterFn>>,
    job: JobEnvelope,
    attempts: u32,
    error: String,
) {
    let cb = dead_letter.lock().await.clone();
    if let Some(cb) = cb {
        let dl = JobDeadLetter {
            name: job.name,
            payload: job.payload,
            attempts,
            error,
        };
        // The callback sees who enqueued the job, as the job did.
        job.context.install(deliver_dead_letter(cb, dl)).await;
    } else {
        tracing::error!(job = job.name, attempts, error = %error, "job dead-lettered");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Pins the documented 1s, 2s, 4s, 8s sequence.
    #[test]
    fn retry_backoff_starts_at_one_second_and_doubles() {
        // `failed_attempt` is 0-based: the run that just failed.
        assert_eq!(
            retry_backoff_ms(0),
            1_000,
            "the first retry waits 1s, not 2s"
        );
        assert_eq!(retry_backoff_ms(1), 2_000);
        assert_eq!(retry_backoff_ms(2), 4_000);
        assert_eq!(retry_backoff_ms(3), 8_000);
    }

    /// The cap, and that it cannot overflow the shift.
    #[test]
    fn retry_backoff_caps_rather_than_overflowing() {
        assert_eq!(retry_backoff_ms(10), 1_024_000);
        assert_eq!(
            retry_backoff_ms(u32::MAX),
            1_024_000,
            "a runaway attempt count must clamp, not shift past 63 and panic"
        );
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct Increment;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[async_trait::async_trait]
    impl Job for Increment {
        const NAME: &'static str = "test:increment";
        async fn run(&self) -> Result<(), JobError> {
            COUNTER.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct AlwaysFail {
        fatal: bool,
    }

    #[async_trait::async_trait]
    impl Job for AlwaysFail {
        const NAME: &'static str = "test:always_fail";
        const MAX_ATTEMPTS: u32 = 2;
        async fn run(&self) -> Result<(), JobError> {
            if self.fatal {
                Err(JobError::Fatal("dead now".into()))
            } else {
                Err(JobError::Retryable("transient".into()))
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct EventuallyOk {
        fail_n: u32,
        success_marker_id: u64,
    }

    static SUCCESSES: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

    #[async_trait::async_trait]
    impl Job for EventuallyOk {
        const NAME: &'static str = "test:eventually_ok";
        const MAX_ATTEMPTS: u32 = 5;
        async fn run(&self) -> Result<(), JobError> {
            let n = ATTEMPTS.fetch_add(1, Ordering::SeqCst);
            if (n as u32) < self.fail_n {
                Err(JobError::Retryable(format!("attempt {n}")))
            } else {
                SUCCESSES.lock().unwrap().push(self.success_marker_id);
                Ok(())
            }
        }
    }

    /// The job records what the enqueuer's context was.
    ///
    /// Before this, a job ran with no ambient context at all — an audit
    /// row written from one recorded `system`, so "who deleted this?"
    /// was a dead end whenever a job did it.
    ///
    #[tokio::test]
    async fn a_job_sees_the_context_of_whoever_enqueued_it() {
        use crate::audit::{current_source, with_source, AuditSource};

        static SEEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

        #[derive(serde::Serialize, serde::Deserialize)]
        struct RecordSource;

        #[async_trait::async_trait]
        impl Job for RecordSource {
            const NAME: &'static str = "record_source";
            async fn run(&self) -> Result<(), JobError> {
                *SEEN.lock().unwrap() = Some(current_source().as_token());
                Ok(())
            }
        }

        let q = InMemoryJobQueue::with_workers(1);
        q.register::<RecordSource>().await;
        q.start().await;

        with_source(AuditSource::User { id: "99".into() }, async {
            q.dispatch(&RecordSource).await.unwrap();
        })
        .await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while SEEN.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        q.shutdown().await;

        assert_eq!(
            SEEN.lock().unwrap().clone(),
            Some("user:99".to_owned()),
            "the job should see who enqueued it"
        );
    }

    /// A job enqueued by nobody in particular stays attributable to the
    /// system — it does not invent an actor.
    #[tokio::test]
    async fn a_job_enqueued_outside_any_scope_is_system() {
        use crate::audit::current_source;

        static SEEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

        #[derive(serde::Serialize, serde::Deserialize)]
        struct RecordPlain;

        #[async_trait::async_trait]
        impl Job for RecordPlain {
            const NAME: &'static str = "record_plain";
            async fn run(&self) -> Result<(), JobError> {
                *SEEN.lock().unwrap() = Some(current_source().as_token());
                Ok(())
            }
        }

        let q = InMemoryJobQueue::with_workers(1);
        q.register::<RecordPlain>().await;
        q.start().await;
        q.dispatch(&RecordPlain).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while SEEN.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        q.shutdown().await;

        assert_eq!(SEEN.lock().unwrap().clone(), Some("system".to_owned()));
    }

    #[tokio::test]
    async fn dispatch_runs_handler() {
        COUNTER.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(2);
        q.register::<Increment>().await;
        q.start().await;
        q.dispatch(&Increment).await.unwrap();
        // Wait briefly for worker
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
        q.shutdown().await;
    }

    #[tokio::test]
    async fn fatal_error_goes_to_dead_letter() {
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<AlwaysFail>().await;
        let captured: Arc<Mutex<Vec<JobDeadLetter>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        q.on_dead_letter(move |dl| {
            let cap = cap.clone();
            async move {
                cap.lock().await.push(dl);
            }
        })
        .await;
        q.start().await;
        q.dispatch(&AlwaysFail { fatal: true }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(captured.lock().await.len(), 1);
        assert!(captured.lock().await[0].error.contains("dead now"));
        q.shutdown().await;
    }

    /// `MAX_ATTEMPTS = 0` runs once, then dead-letters its error (#2333).
    #[tokio::test]
    async fn zero_max_attempts_runs_once() {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        #[derive(Serialize, Deserialize)]
        struct NoAttempts;
        #[async_trait::async_trait]
        impl Job for NoAttempts {
            const NAME: &'static str = "test:no_attempts";
            const MAX_ATTEMPTS: u32 = 0;
            async fn run(&self) -> Result<(), JobError> {
                RUNS.fetch_add(1, Ordering::SeqCst);
                Err(JobError::Retryable("again".into()))
            }
        }
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<NoAttempts>().await;
        let captured: Arc<std::sync::Mutex<Vec<JobDeadLetter>>> = Arc::default();
        let cap = captured.clone();
        q.on_dead_letter(move |dl| {
            cap.lock().unwrap().push(dl);
            async {}
        })
        .await;
        q.start().await;
        q.dispatch(&NoAttempts).await.unwrap();
        wait_until("the dead letter", || !captured.lock().unwrap().is_empty()).await;
        q.shutdown().await;
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);
        let dl = captured.lock().unwrap();
        assert_eq!((dl[0].attempts, dl[0].error.as_str()), (1, "again"));
    }

    /// #1229 — the dead-letter callback runs as the job's enqueuer.
    #[tokio::test]
    async fn dead_letter_callback_sees_the_enqueuer() {
        use crate::audit::{current_source, with_source, AuditSource};
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<AlwaysFail>().await;
        let seen: Arc<Mutex<Option<String>>> = Arc::default();
        let s = seen.clone();
        q.on_dead_letter(move |_dl| {
            let s = s.clone();
            async move {
                *s.lock().await = Some(current_source().as_token());
            }
        })
        .await;
        q.start().await;
        with_source(AuditSource::User { id: "5".into() }, async {
            q.dispatch(&AlwaysFail { fatal: true }).await.unwrap();
        })
        .await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while seen.lock().await.is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        q.shutdown().await;
        assert_eq!(seen.lock().await.as_deref(), Some("user:5"));
    }

    #[tokio::test]
    async fn retryable_succeeds_eventually() {
        ATTEMPTS.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<EventuallyOk>().await;
        q.start().await;
        let marker = 12345;
        q.dispatch(&EventuallyOk {
            fail_n: 2,
            success_marker_id: marker,
        })
        .await
        .unwrap();
        // Backoff is ~1s then ~2s; 7s leaves plenty of room.
        tokio::time::sleep(Duration::from_millis(7000)).await;
        let succ = SUCCESSES.lock().unwrap();
        assert!(succ.contains(&marker), "expected marker, got {succ:?}");
        drop(succ);
        q.shutdown().await;
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct Panics;

    #[async_trait::async_trait]
    impl Job for Panics {
        const NAME: &'static str = "test:panics";
        const MAX_ATTEMPTS: u32 = 1;
        async fn run(&self) -> Result<(), JobError> {
            panic!("boom");
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct AfterPanic;

    static AFTER_PANIC: AtomicUsize = AtomicUsize::new(0);

    #[async_trait::async_trait]
    impl Job for AfterPanic {
        const NAME: &'static str = "test:after_panic";
        async fn run(&self) -> Result<(), JobError> {
            AFTER_PANIC.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// #1843: the one worker survives a panic, and the panic is a failed run.
    #[tokio::test]
    async fn a_panicking_job_keeps_the_worker() {
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<Panics>().await;
        q.register::<AfterPanic>().await;
        let dead: Arc<Mutex<Vec<JobDeadLetter>>> = Arc::default();
        let d = dead.clone();
        q.on_dead_letter(move |dl| {
            let d = d.clone();
            async move { d.lock().await.push(dl) }
        })
        .await;
        q.start().await;
        q.dispatch(&Panics).await.unwrap();
        q.dispatch(&AfterPanic).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(AFTER_PANIC.load(Ordering::SeqCst), 1, "worker still runs");
        let dead = dead.lock().await;
        assert!(dead[0].error.contains("job panicked: boom"), "{dead:?}");
        assert_eq!(q.pending_count().await, 0);
        q.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_job_is_logged_not_panic() {
        // Dispatch a job whose NAME isn't registered — the worker should
        // skip it without crashing.
        #[derive(Serialize, Deserialize)]
        struct UnregisteredJob;

        #[async_trait::async_trait]
        impl Job for UnregisteredJob {
            const NAME: &'static str = "test:unregistered";
            async fn run(&self) -> Result<(), JobError> {
                Ok(())
            }
        }

        let q = InMemoryJobQueue::with_workers(1);
        // Deliberately don't register
        q.start().await;
        q.dispatch(&UnregisteredJob).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // No panic = pass
        q.shutdown().await;
    }

    #[tokio::test]
    async fn pending_count_tracks_in_flight() {
        let q = InMemoryJobQueue::with_workers(0); // no workers — jobs queue but don't run
        q.register::<Increment>().await;
        for _ in 0..3 {
            q.dispatch(&Increment).await.unwrap();
        }
        assert_eq!(q.pending_count().await, 3);
    }

    /// Sleeps `ms` on its first run, then counts a finish in `DRAINED`.
    #[derive(Serialize, Deserialize, Debug)]
    struct SlowOnce {
        ms: u64,
    }

    static DRAIN_RUNS: AtomicUsize = AtomicUsize::new(0);
    static DRAINED: AtomicUsize = AtomicUsize::new(0);

    #[async_trait::async_trait]
    impl Job for SlowOnce {
        const NAME: &'static str = "test:slow_once";
        async fn run(&self) -> Result<(), JobError> {
            if DRAIN_RUNS.fetch_add(1, Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(self.ms)).await;
            }
            DRAINED.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Fails retryably on its first run, then succeeds.
    #[derive(Serialize, Deserialize, Debug)]
    struct RetryOnce;

    static RETRY_RUNS: AtomicUsize = AtomicUsize::new(0);

    #[async_trait::async_trait]
    impl Job for RetryOnce {
        const NAME: &'static str = "test:retry_once";
        async fn run(&self) -> Result<(), JobError> {
            if RETRY_RUNS.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(JobError::Retryable("once".into()));
            }
            Ok(())
        }
    }

    async fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // The tests below share the statics above.
    static LIFECYCLE: Mutex<()> = Mutex::const_new(());

    /// #1255: shutdown lets a running job finish instead of aborting it.
    #[tokio::test]
    async fn shutdown_drains_a_running_job() {
        let _g = LIFECYCLE.lock().await;
        DRAIN_RUNS.store(0, Ordering::SeqCst);
        DRAINED.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<SlowOnce>().await;
        q.start().await;
        q.dispatch(&SlowOnce { ms: 300 }).await.unwrap();
        wait_until("the run to start", || {
            DRAIN_RUNS.load(Ordering::SeqCst) == 1
        })
        .await;
        q.shutdown().await;
        assert_eq!(DRAINED.load(Ordering::SeqCst), 1, "the job finished");
        assert_eq!(q.pending_count().await, 0);
    }

    /// #1255: a job past the grace period is aborted and re-queued.
    #[tokio::test]
    async fn a_job_past_the_grace_is_requeued() {
        let _g = LIFECYCLE.lock().await;
        DRAIN_RUNS.store(0, Ordering::SeqCst);
        DRAINED.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1).shutdown_grace(Duration::from_millis(50));
        q.register::<SlowOnce>().await;
        q.start().await;
        q.dispatch(&SlowOnce { ms: 30_000 }).await.unwrap();
        wait_until("the run to start", || {
            DRAIN_RUNS.load(Ordering::SeqCst) == 1
        })
        .await;
        q.shutdown().await;
        assert_eq!(q.pending_count().await, 1, "aborted, not lost");
        q.start().await;
        wait_until("the rerun", || DRAINED.load(Ordering::SeqCst) == 1).await;
        wait_until("the count to settle", || {
            q.pending.load(Ordering::SeqCst) == 0
        })
        .await;
        q.shutdown().await;
    }

    /// #1255: a retry parked in backoff survives shutdown and runs on restart.
    #[tokio::test]
    async fn a_parked_retry_survives_shutdown() {
        let _g = LIFECYCLE.lock().await;
        RETRY_RUNS.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<RetryOnce>().await;
        q.start().await;
        q.dispatch(&RetryOnce).await.unwrap();
        wait_until("the failed run", || RETRY_RUNS.load(Ordering::SeqCst) == 1).await;
        q.shutdown().await;
        assert_eq!(q.pending_count().await, 1, "the retry is still queued");
        q.start().await;
        wait_until("the retry", || RETRY_RUNS.load(Ordering::SeqCst) == 2).await;
        wait_until("the count to reach 0", || {
            q.pending.load(Ordering::SeqCst) == 0
        })
        .await;
        q.shutdown().await;
    }

    /// #1677: `start` after `shutdown` runs jobs again.
    #[tokio::test]
    async fn start_after_shutdown_runs_jobs() {
        let _g = LIFECYCLE.lock().await;
        DRAIN_RUNS.store(1, Ordering::SeqCst); // skip the slow first run
        DRAINED.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<SlowOnce>().await;
        q.start().await;
        q.shutdown().await;
        q.start().await;
        q.dispatch(&SlowOnce { ms: 0 }).await.unwrap();
        wait_until("the job after a restart", || {
            DRAINED.load(Ordering::SeqCst) == 1
        })
        .await;
        q.shutdown().await;
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct Marker;

    static MARKED: AtomicUsize = AtomicUsize::new(0);

    #[async_trait::async_trait]
    impl Job for Marker {
        const NAME: &'static str = "test:marker";
        async fn run(&self) -> Result<(), JobError> {
            MARKED.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A one-run job that hangs; counts its runs.
    #[derive(Serialize, Deserialize, Debug)]
    struct HangOnce;

    static HANG_RUNS: AtomicUsize = AtomicUsize::new(0);

    #[async_trait::async_trait]
    impl Job for HangOnce {
        const NAME: &'static str = "test:hang_once";
        const MAX_ATTEMPTS: u32 = 1;
        async fn run(&self) -> Result<(), JobError> {
            HANG_RUNS.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }

    /// A job dispatched while shutdown drains stays queued for the next start.
    #[tokio::test]
    async fn dispatch_during_drain_stays_queued() {
        let _g = LIFECYCLE.lock().await;
        DRAIN_RUNS.store(0, Ordering::SeqCst);
        MARKED.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<SlowOnce>().await;
        q.register::<Marker>().await;
        q.start().await;
        q.dispatch(&SlowOnce { ms: 300 }).await.unwrap();
        wait_until("the run to start", || {
            DRAIN_RUNS.load(Ordering::SeqCst) == 1
        })
        .await;
        tokio::join!(q.shutdown(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            q.dispatch(&Marker).await.unwrap();
        });
        assert_eq!(MARKED.load(Ordering::SeqCst), 0, "not run while stopping");
        assert_eq!(q.pending_count().await, 1);
        q.start().await;
        wait_until("the queued job", || MARKED.load(Ordering::SeqCst) == 1).await;
        q.shutdown().await;
    }

    /// A parked retry keeps its backoff across a restart.
    #[tokio::test]
    async fn a_parked_retry_keeps_its_backoff_across_restart() {
        let _g = LIFECYCLE.lock().await;
        RETRY_RUNS.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<RetryOnce>().await;
        q.start().await;
        q.dispatch(&RetryOnce).await.unwrap();
        wait_until("the failed run", || RETRY_RUNS.load(Ordering::SeqCst) == 1).await;
        q.shutdown().await;
        q.start().await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            RETRY_RUNS.load(Ordering::SeqCst),
            1,
            "the 1s backoff still holds"
        );
        wait_until("the retry", || RETRY_RUNS.load(Ordering::SeqCst) == 2).await;
        q.shutdown().await;
    }

    /// An aborted run spends its attempt, as on the DB queue.
    #[tokio::test]
    async fn an_aborted_run_spends_its_attempt() {
        let _g = LIFECYCLE.lock().await;
        HANG_RUNS.store(0, Ordering::SeqCst);
        let q = InMemoryJobQueue::with_workers(1).shutdown_grace(Duration::from_millis(50));
        q.register::<HangOnce>().await;
        let dead: Arc<Mutex<Vec<JobDeadLetter>>> = Arc::default();
        let d = dead.clone();
        q.on_dead_letter(move |dl| {
            let d = d.clone();
            async move { d.lock().await.push(dl) }
        })
        .await;
        q.start().await;
        q.dispatch(&HangOnce).await.unwrap();
        wait_until("the run to start", || HANG_RUNS.load(Ordering::SeqCst) == 1).await;
        q.shutdown().await;
        q.start().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while dead.lock().await.is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "no dead letter");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(HANG_RUNS.load(Ordering::SeqCst), 1, "not run again");
        assert_eq!(q.pending_count().await, 0);
        q.shutdown().await;
    }

    /// `inmemory_from_settings` uses `concurrency` when it is set.
    #[cfg(feature = "config")]
    #[test]
    fn inmemory_from_settings_uses_configured_concurrency() {
        let mut s = crate::config::JobsSettings::default();
        s.concurrency = Some(8);
        let q = inmemory_from_settings(&s);
        assert_eq!(q.worker_count, 8);
    }

    #[cfg(feature = "config")]
    #[test]
    fn inmemory_from_settings_defaults_to_four_workers() {
        let s = crate::config::JobsSettings::default();
        let q = inmemory_from_settings(&s);
        assert_eq!(q.worker_count, 4);
    }
}
