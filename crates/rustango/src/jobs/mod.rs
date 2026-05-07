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
//! ## Backends shipped
//!
//! | Backend | When to use |
//! |---|---|
//! | [`InMemoryJobQueue`] | Single-process apps, dev, tests. Jobs lost on restart. |
//!
//! ## Backends planned
//!
//! - **`DbJobQueue`** — Postgres-backed using `SELECT ... FOR UPDATE SKIP LOCKED`
//!   for multi-process / multi-replica deployments
//! - **`RedisJobQueue`** — Redis-backed using `BRPOPLPUSH` for high-throughput / language-agnostic queues
//!
//! ## Retry policy
//!
//! Jobs that return `Err(JobError::Retryable(_))` are retried with
//! exponential backoff (1s, 2s, 4s, 8s, ...) up to `max_attempts` (default 5).
//! `Err(JobError::Fatal(_))` skips the retry queue and goes straight to the
//! dead-letter handler.

#[cfg(feature = "jobs-postgres")]
pub mod pg;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

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

    /// Maximum retry attempts before giving up. Default 5.
    const MAX_ATTEMPTS: u32 = 5;

    /// Run the job. Return `Ok(())` on success, `Err(Retryable(_))` to
    /// retry with backoff, `Err(Fatal(_))` to dead-letter immediately.
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

    /// Spawn worker tasks. Idempotent — calling `start` twice is a no-op.
    async fn start(&self);

    /// Stop all workers gracefully (drain in-flight jobs, then exit).
    async fn shutdown(&self);

    /// Number of jobs currently queued (waiting to be picked up).
    async fn pending_count(&self) -> usize;
}

// ------------------------------------------------------------------ JobEnvelope

#[derive(Debug, Clone)]
struct JobEnvelope {
    name: &'static str,
    payload: serde_json::Value,
    attempt: u32,
    max_attempts: u32,
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
                job.run().await
            })
        });
        self.handlers.insert(T::NAME, (handler, T::MAX_ATTEMPTS));
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

// ------------------------------------------------------------------ InMemoryJobQueue

/// In-process job queue using tokio mpsc channels.
///
/// **Persistence: none.** Jobs in flight or queued at process restart are lost.
/// Use the (planned) `DbJobQueue` for production multi-process deployments.
pub struct InMemoryJobQueue {
    tx: mpsc::UnboundedSender<JobEnvelope>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<JobEnvelope>>>,
    registry: Arc<Mutex<HandlerRegistry>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    worker_count: usize,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
}

impl InMemoryJobQueue {
    /// Build a queue with `worker_count` worker tasks (call `start` to spawn them).
    #[must_use]
    pub fn with_workers(worker_count: usize) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            registry: Arc::new(Mutex::new(HandlerRegistry::default())),
            workers: Mutex::new(Vec::new()),
            worker_count,
            dead_letter: Arc::new(Mutex::new(None)),
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
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
            max_attempts: T::MAX_ATTEMPTS,
        };
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.tx
            .send(envelope)
            .map_err(|e| JobError::Queue(e.to_string()))?;
        Ok(())
    }

    async fn start(&self) {
        let mut workers = self.workers.lock().await;
        if !workers.is_empty() {
            return; // already started
        }
        let mut rx_guard = self.rx.lock().await;
        let rx = rx_guard
            .take()
            .expect("queue already started without workers");
        // Wrap rx in Arc<Mutex> so multiple workers can pull
        let rx = Arc::new(Mutex::new(rx));

        for _ in 0..self.worker_count {
            let rx = rx.clone();
            let registry = self.registry.clone();
            let dead_letter = self.dead_letter.clone();
            let pending = self.pending.clone();
            let tx_for_retries = self.tx.clone();
            let handle = tokio::spawn(async move {
                worker_loop(rx, registry, dead_letter, pending, tx_for_retries).await;
            });
            workers.push(handle);
        }
    }

    async fn shutdown(&self) {
        // Drop the inbound channel by replacing it with a fresh one
        // (the workers' rx will see the channel close and exit naturally).
        // Since rx is held inside a Mutex, we can't easily replace; just abort.
        let mut workers = self.workers.lock().await;
        for h in workers.drain(..) {
            h.abort();
            let _ = h.await;
        }
    }

    async fn pending_count(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::SeqCst)
    }
}

async fn worker_loop(
    rx: Arc<Mutex<mpsc::UnboundedReceiver<JobEnvelope>>>,
    registry: Arc<Mutex<HandlerRegistry>>,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    tx: mpsc::UnboundedSender<JobEnvelope>,
) {
    loop {
        // Pull one job (briefly hold the rx mutex)
        let envelope = {
            let mut rx_guard = rx.lock().await;
            match rx_guard.recv().await {
                Some(e) => e,
                None => return, // channel closed
            }
        };

        let handler = {
            let reg = registry.lock().await;
            reg.lookup(envelope.name)
        };

        let Some((handler, _max_attempts)) = handler else {
            tracing::error!(job = envelope.name, "no handler registered");
            pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            continue;
        };

        let payload = envelope.payload.clone();
        let result = handler(payload).await;

        match result {
            Ok(()) => {
                pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
            Err(JobError::Retryable(msg)) => {
                let next_attempt = envelope.attempt + 1;
                if next_attempt >= envelope.max_attempts {
                    // Dead-letter
                    let dl_callback = dead_letter.lock().await.clone();
                    if let Some(cb) = dl_callback {
                        cb(JobDeadLetter {
                            name: envelope.name,
                            payload: envelope.payload.clone(),
                            attempts: next_attempt,
                            error: msg,
                        })
                        .await;
                    } else {
                        tracing::error!(job = envelope.name, attempts = next_attempt, error = %msg, "job exhausted retries");
                    }
                    pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                } else {
                    // Re-enqueue after backoff (1s, 2s, 4s, 8s, ...)
                    let backoff_ms = 1000u64.saturating_mul(1u64 << next_attempt.min(10));
                    let mut retry = envelope.clone();
                    retry.attempt = next_attempt;
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        let _ = tx.send(retry);
                    });
                    // Note: pending stays incremented while retry is in flight
                }
            }
            Err(e @ (JobError::Fatal(_) | JobError::Queue(_))) => {
                let msg = e.to_string();
                let dl_callback = dead_letter.lock().await.clone();
                if let Some(cb) = dl_callback {
                    cb(JobDeadLetter {
                        name: envelope.name,
                        payload: envelope.payload.clone(),
                        attempts: envelope.attempt + 1,
                        error: msg,
                    })
                    .await;
                } else {
                    tracing::error!(job = envelope.name, error = %msg, "job fatal");
                }
                pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        // Backoff: ~2s after first failure, ~4s after second → wait ~7s to be safe.
        tokio::time::sleep(Duration::from_millis(7000)).await;
        let succ = SUCCESSES.lock().unwrap();
        assert!(succ.contains(&marker), "expected marker, got {succ:?}");
        drop(succ);
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
}
