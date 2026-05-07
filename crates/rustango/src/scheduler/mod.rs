//! In-process scheduled task runner — fire async jobs at fixed intervals.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::scheduler::Scheduler;
//! use std::time::Duration;
//!
//! let scheduler = Scheduler::new();
//!
//! scheduler.every("cleanup_expired_sessions", Duration::from_secs(300), || async {
//!     cleanup().await.ok();
//! });
//!
//! scheduler.every("rotate_logs", Duration::from_secs(86_400), || async {
//!     rotate().await.ok();
//! });
//!
//! // Spawn the runner — runs until the returned handle is dropped
//! let handle = scheduler.start();
//! // ... app runs ...
//! handle.shutdown().await;
//! ```
//!
//! ## Semantics
//!
//! - **Per-task tick loops**: each registered task runs in its own tokio
//!   task with a `tokio::time::interval`.
//! - **Drift handling**: if a job takes longer than the interval, ticks
//!   are skipped (default `MissedTickBehavior::Skip`) — no ticks pile up.
//! - **First fire**: occurs after one full interval, not immediately.
//! - **Panic isolation**: a panicking job aborts only that task; other
//!   scheduled jobs keep running. The panic is logged via `tracing::error!`.
//! - **Shutdown**: `Handle::shutdown()` aborts every spawned task.
//!
//! ## Production note
//!
//! For multi-process deployments where the same job must run on exactly one
//! node (not per-replica), pair this with the cache layer for distributed
//! locks, or use an external scheduler (Kubernetes CronJob, GitHub Actions).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};

/// Async job factory — takes no args, returns a `Future`. The factory is
/// called once per tick to produce a fresh future.
type JobFactory = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct Task {
    name: String,
    period: Duration,
    factory: JobFactory,
}

/// Scheduler configuration — register tasks, then `start()` to spawn the runner.
pub struct Scheduler {
    tasks: Mutex<Vec<Task>>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// New empty scheduler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Register a task to run every `period`. The first invocation occurs
    /// after one full period (not immediately at start).
    ///
    /// `name` appears in tracing logs and panic messages — keep it short
    /// and identifying.
    pub fn every<F, Fut>(&self, name: &str, period: Duration, job: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let factory: JobFactory = Arc::new(move || Box::pin(job()));
        self.tasks
            .lock()
            .expect("scheduler tasks poisoned")
            .push(Task {
                name: name.to_owned(),
                period,
                factory,
            });
    }

    /// Number of registered tasks (not yet running).
    #[must_use]
    pub fn task_count(&self) -> usize {
        self.tasks.lock().expect("scheduler tasks poisoned").len()
    }

    /// Spawn the runner — one tokio task per registered job. Returns a
    /// [`Handle`] for graceful shutdown.
    pub fn start(self) -> Handle {
        let tasks = self.tasks.into_inner().expect("scheduler tasks poisoned");
        let mut handles = Vec::with_capacity(tasks.len());
        for t in tasks {
            handles.push(spawn_task_loop(t));
        }
        Handle { handles }
    }
}

fn spawn_task_loop(task: Task) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = interval(task.period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // First tick fires immediately — drain it so the first ACTUAL run
        // happens after one full period.
        tick.tick().await;
        loop {
            tick.tick().await;
            let factory = task.factory.clone();
            let name = task.name.clone();
            // Run each invocation as a separate spawned task so a panic
            // doesn't kill the loop.
            let job_handle = tokio::spawn(async move {
                let fut = (factory)();
                fut.await;
            });
            if let Err(e) = job_handle.await {
                if e.is_panic() {
                    tracing::error!(task = %name, "scheduled job panicked");
                }
            }
        }
    })
}

/// Handle to a running scheduler — drop or call `shutdown()` to stop.
pub struct Handle {
    handles: Vec<JoinHandle<()>>,
}

impl Handle {
    /// Number of currently-running task loops.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.handles.len()
    }

    /// Abort every running task. After this call no more ticks fire.
    pub async fn shutdown(mut self) {
        let handles = std::mem::take(&mut self.handles);
        for h in handles {
            h.abort();
            let _ = h.await; // ignore JoinError from abort
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        for h in &self.handles {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn task_count_tracks_registrations() {
        let s = Scheduler::new();
        s.every("a", Duration::from_secs(1), || async {});
        s.every("b", Duration::from_secs(1), || async {});
        s.every("c", Duration::from_secs(1), || async {});
        assert_eq!(s.task_count(), 3);
    }

    #[tokio::test]
    async fn job_fires_after_one_period() {
        let counter = Arc::new(AtomicUsize::new(0));
        let s = Scheduler::new();
        let c = counter.clone();
        s.every("count", Duration::from_millis(20), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        let handle = s.start();
        // Wait for ~3 ticks (60ms gives us 3 fires at 20ms intervals)
        tokio::time::sleep(Duration::from_millis(70)).await;
        let count = counter.load(Ordering::SeqCst);
        assert!(count >= 2, "expected at least 2 fires, got {count}");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_further_fires() {
        let counter = Arc::new(AtomicUsize::new(0));
        let s = Scheduler::new();
        let c = counter.clone();
        s.every("stop", Duration::from_millis(15), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        let handle = s.start();
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.shutdown().await;
        let after_shutdown = counter.load(Ordering::SeqCst);
        // Wait — counter must not increase
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(counter.load(Ordering::SeqCst), after_shutdown);
    }

    #[tokio::test]
    async fn panicking_job_does_not_kill_loop() {
        let counter = Arc::new(AtomicUsize::new(0));
        let s = Scheduler::new();
        let c = counter.clone();
        let panic_on_first = Arc::new(AtomicUsize::new(0));
        let p = panic_on_first.clone();
        s.every("flaky", Duration::from_millis(20), move || {
            let c = c.clone();
            let p = p.clone();
            async move {
                let n = p.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    panic!("simulated job failure");
                }
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        let handle = s.start();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let count = counter.load(Ordering::SeqCst);
        assert!(
            count >= 1,
            "loop must keep running after a panic, got count={count}"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn multiple_tasks_run_independently() {
        let a_count = Arc::new(AtomicUsize::new(0));
        let b_count = Arc::new(AtomicUsize::new(0));
        let s = Scheduler::new();
        let a = a_count.clone();
        let b = b_count.clone();
        s.every("a", Duration::from_millis(15), move || {
            let a = a.clone();
            async move {
                a.fetch_add(1, Ordering::SeqCst);
            }
        });
        s.every("b", Duration::from_millis(15), move || {
            let b = b.clone();
            async move {
                b.fetch_add(1, Ordering::SeqCst);
            }
        });
        let handle = s.start();
        assert_eq!(handle.running_count(), 2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(a_count.load(Ordering::SeqCst) >= 1);
        assert!(b_count.load(Ordering::SeqCst) >= 1);
        handle.shutdown().await;
    }
}
