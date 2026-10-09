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
//! - **Zero period is clamped**: `every(_, Duration::ZERO, _)` would
//!   panic tokio's timer at spawn; it is clamped to 1s with a warning
//!   instead (#1256).
//! - **Context**: each tick runs with the audit source and timezone that
//!   were active when the task was registered with `every()` (#1229).
//! - **Shutdown**: `Handle::shutdown()` stops every task loop *and*
//!   aborts any job currently in flight — a running job does not outlive
//!   shutdown (#1256).
//!
//! ## Production note
//!
//! For multi-process deployments where the same job must run on exactly one
//! node (not per-replica), tick often (say every 60s) and wrap the body in
//! [`DistributedLock::once_per_period`] with the real period, or use an
//! external scheduler (Kubernetes CronJob, GitHub Actions). `with_lock`
//! is not enough, and neither is ticking once per period: each pod ticks
//! from its own start time.
//!
//! [`DistributedLock::once_per_period`]: crate::distributed_lock::DistributedLock::once_per_period

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
    /// Captured at `every()`: a tick has no caller to inherit from.
    context: crate::task_context::TaskContext,
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
    ///
    /// **Each tick runs with the audit source and timezone active at
    /// this call** (#1229), so wrap registration in
    /// [`crate::audit::with_source`] to attribute a task, e.g.
    /// `AuditSource::Custom("cron:sweep".into())`. Outside any scope that
    /// is `System` and UTC. There is no session or tenant: a scheduled sweep
    /// over per-tenant tables must fan out explicitly with
    /// [`crate::tenancy::for_each_tenant`] (#1226), and a
    /// once-per-cluster guard should be scoped per tenant with
    /// [`crate::distributed_lock::DistributedLock::for_tenant`] (#1228).
    pub fn every<F, Fut>(&self, name: &str, period: Duration, job: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // A zero period panics `tokio::time::interval` at spawn time,
        // which would silently kill this task's loop (#1256). A zero
        // interval is always a caller mistake — clamp it to 1s and warn
        // rather than take the loop down.
        let period = if period.is_zero() {
            tracing::warn!(
                target: "rustango::scheduler",
                task = %name,
                "every() called with a zero period; clamping to 1s (a zero \
                 interval panics tokio's timer)",
            );
            Duration::from_secs(1)
        } else {
            period
        };
        let factory: JobFactory = Arc::new(move || Box::pin(job()));
        self.tasks
            .lock()
            .expect("scheduler tasks poisoned")
            .push(Task {
                name: name.to_owned(),
                period,
                factory,
                context: crate::task_context::TaskContext::capture(),
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

/// Aborts the wrapped task when dropped. A bare `JoinHandle` detaches on
/// drop (the task keeps running); this makes dropping it stop the task,
/// so aborting a scheduler loop stops any job it has in flight (#1256).
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
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
            let context = task.context.clone();
            let name = task.name.clone();
            // Run each invocation as a separate spawned task so a panic
            // doesn't kill the loop. Wrap it in an abort-on-drop guard so
            // that when the loop itself is aborted (`Handle::shutdown`),
            // an in-flight job is aborted with it rather than left
            // running detached (#1256) — dropping a bare `JoinHandle`
            // does NOT stop its task.
            let mut job = AbortOnDrop(tokio::spawn(async move {
                context.install(async move { (factory)().await }).await;
            }));
            if let Err(e) = (&mut job.0).await {
                if e.is_panic() {
                    tracing::error!(task = %name, "scheduled job panicked");
                }
            }
            // Normal completion: `job` drops here and aborts an
            // already-finished task, which is a documented no-op.
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

    /// #1256 — a zero period must not panic the task loop. Before the
    /// clamp, `interval(Duration::ZERO)` panicked at spawn, silently
    /// killing this task; now it is clamped to 1s with a warning, so the
    /// scheduler starts and runs.
    #[tokio::test]
    async fn zero_period_does_not_panic() {
        let s = Scheduler::new();
        s.every("zero", Duration::ZERO, || async {});
        let handle = s.start();
        assert_eq!(handle.running_count(), 1, "loop must be running, not dead");
        handle.shutdown().await;
    }

    /// #1256 — `shutdown()` must stop a job that is in flight, not leave
    /// it running detached. A long job increments a flag only if it is
    /// allowed to finish; shutting down mid-run must prevent that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_aborts_in_flight_job() {
        use std::sync::Arc;
        let finished = Arc::new(AtomicUsize::new(0));
        let f = finished.clone();
        let s = Scheduler::new();
        // Fire quickly, then the job sleeps well past our shutdown.
        s.every("slow", Duration::from_millis(50), move || {
            let f = f.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                f.fetch_add(1, Ordering::SeqCst);
            }
        });
        let handle = s.start();
        // Let the first tick fire and the job start, then shut down while
        // it is still sleeping.
        tokio::time::sleep(Duration::from_millis(120)).await;
        handle.shutdown().await;
        // Give the (aborted) job more than its sleep to prove it did NOT
        // complete.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            finished.load(Ordering::SeqCst),
            0,
            "in-flight job kept running after shutdown",
        );
    }

    /// #1229 — a tick runs with the context active at `every()`, not the
    /// `System` a bare spawn would give it; outside a scope it stays `System`.
    #[tokio::test]
    async fn tick_runs_with_the_context_active_at_registration() {
        use crate::audit::{current_source, with_source, AuditSource};
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s = Scheduler::new();
        let (a, b) = (seen.clone(), seen.clone());
        with_source(AuditSource::Custom("cron:sweep".into()), async {
            s.every("attributed", Duration::from_millis(10), move || {
                a.lock().unwrap().push(current_source().as_token());
                async {}
            });
        })
        .await;
        s.every("plain", Duration::from_millis(10), move || {
            let b = b.clone();
            async move { b.lock().unwrap().push(current_source().as_token()) }
        });
        let handle = s.start();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while seen.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.shutdown().await;
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"cron:sweep".to_owned()), "got {seen:?}");
        assert!(seen.contains(&"system".to_owned()), "got {seen:?}");
    }

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
        // Poll up to 2 seconds for at least 2 fires. Earlier the test
        // used a fixed 70ms sleep, expecting 3 fires at the 20ms
        // cadence — but CI's busy runner can deliver the first tick
        // 50ms+ late, leaving the assertion at 1 fire and red. Polling
        // converges as soon as the assertion holds; the upper bound
        // is generous so we don't false-fail under heavy load.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if counter.load(Ordering::SeqCst) >= 2 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
        // Poll up to 2s for the post-panic tick to land. Earlier we
        // slept a fixed 80ms — CI's busy runner sometimes takes 100ms+
        // to deliver the second tick after the panicking one, so the
        // assertion landed at 0 and red-tagged main. Polling converges
        // as soon as `counter >= 1`. Same shape as the on_commit_live /
        // jobs_pg_live / job_fires_after_one_period flake fixes.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
        // Same poll-loop pattern as the other timing tests in this
        // file — CI's busy runner can blow well past a fixed 50ms
        // sleep before delivering the first tick.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if a_count.load(Ordering::SeqCst) >= 1 && b_count.load(Ordering::SeqCst) >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(a_count.load(Ordering::SeqCst) >= 1);
        assert!(b_count.load(Ordering::SeqCst) >= 1);
        handle.shutdown().await;
    }

    /// The documented pattern across a rolling deploy: pod B boots after
    /// pod A stops. Every window runs exactly once, none is skipped (#2330).
    #[cfg(feature = "cache")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn staggered_pods_run_a_locked_job_once_per_window() {
        use crate::cache::{BoxedCache, InMemoryCache};
        use crate::distributed_lock::DistributedLock;
        const PERIOD: Duration = Duration::from_millis(100);
        // Ticks well inside the window; ticking once per period skips windows.
        const TICK: Duration = Duration::from_millis(10);
        let window = || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            now.as_millis() / PERIOD.as_millis()
        };
        let cache: BoxedCache = Arc::new(InMemoryCache::new());
        let ran = Arc::new(Mutex::new(Vec::new()));
        let pod = || {
            let s = Scheduler::new();
            let lock = DistributedLock::new(cache.clone());
            let ran = ran.clone();
            s.every("daily_report", TICK, move || {
                let (lock, ran) = (lock.clone(), ran.clone());
                async move {
                    // What `once_per_period` does, with the window kept for the record.
                    let w = window();
                    lock.once_in_window("daily_report", PERIOD, w, || async {
                        ran.lock().unwrap().push(w);
                        Ok::<_, ()>(())
                    })
                    .await;
                }
            });
            s.start()
        };
        let a = pod();
        tokio::time::sleep(Duration::from_millis(470)).await;
        a.shutdown().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let b = pod();
        tokio::time::sleep(Duration::from_millis(600)).await;
        b.shutdown().await;
        let ran = ran.lock().unwrap().clone();
        assert!(ran.len() >= 8, "too few runs: {ran:?}");
        let expected: Vec<u128> = (ran[0]..=ran[ran.len() - 1]).collect();
        assert_eq!(ran, expected, "a window ran twice or not at all");
    }
}
