//! Backing test for `docs/jobs.md` — the in-memory queue, retry-with-backoff,
//! and the dead-letter handler. The persistent (DB-backed) path is dogfooded
//! separately by `jobs_sqlite_live.rs` / `jobs_pg_live.rs`.
//!
//! Run: `cargo test -p rustango --test jobs_doc`

#![cfg(feature = "jobs")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustango::jobs::{InMemoryJobQueue, Job, JobError, JobQueue};
use serde::{Deserialize, Serialize};

/// Poll `pred` until true or a generous timeout (jobs run on worker tasks).
async fn wait_until(pred: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !pred() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ----------------------------------------------------------- 1. dispatch + run

static WELCOME_RUNS: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct WelcomeEmail {
    user_id: i64,
}

#[async_trait::async_trait]
impl Job for WelcomeEmail {
    const NAME: &'static str = "doc:welcome_email";
    async fn run(&self) -> Result<(), JobError> {
        assert!(self.user_id > 0, "payload round-tripped");
        WELCOME_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn dispatched_jobs_run_on_workers() {
    let queue = Arc::new(InMemoryJobQueue::with_workers(4));
    queue.register::<WelcomeEmail>().await; // register before start/dispatch
    queue.start().await;

    for user_id in 1..=3 {
        queue.dispatch(&WelcomeEmail { user_id }).await.unwrap();
    }

    wait_until(|| WELCOME_RUNS.load(Ordering::SeqCst) == 3).await;
    queue.shutdown().await; // drains in-flight, then stops workers
    assert_eq!(WELCOME_RUNS.load(Ordering::SeqCst), 3);
}

// ------------------------------------------------ 2. retryable failure + backoff

static FLAKY_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static FLAKY_SUCCEEDED: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct FlakyImport;

#[async_trait::async_trait]
impl Job for FlakyImport {
    const NAME: &'static str = "doc:flaky_import";
    async fn run(&self) -> Result<(), JobError> {
        let attempt = FLAKY_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt < 2 {
            // Transient failure → the worker retries with exponential backoff.
            Err(JobError::Retryable(format!("attempt {attempt} failed")))
        } else {
            FLAKY_SUCCEEDED.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}

#[tokio::test]
async fn retryable_failure_is_retried_until_it_succeeds() {
    let queue = Arc::new(InMemoryJobQueue::with_workers(2));
    queue.register::<FlakyImport>().await;
    queue.start().await;

    queue.dispatch(&FlakyImport).await.unwrap();

    wait_until(|| FLAKY_SUCCEEDED.load(Ordering::SeqCst) == 1).await;
    queue.shutdown().await;
    assert_eq!(
        FLAKY_SUCCEEDED.load(Ordering::SeqCst),
        1,
        "eventually succeeded"
    );
    assert!(
        FLAKY_ATTEMPTS.load(Ordering::SeqCst) >= 2,
        "ran more than once (retried)"
    );
}

// ----------------------------------------------------- 3. fatal → dead letter

#[derive(Serialize, Deserialize)]
struct BadPayload;

#[async_trait::async_trait]
impl Job for BadPayload {
    const NAME: &'static str = "doc:bad_payload";
    async fn run(&self) -> Result<(), JobError> {
        // Permanent failure → skip retries, go straight to dead-letter.
        Err(JobError::Fatal("unprocessable".into()))
    }
}

#[tokio::test]
async fn fatal_job_goes_to_the_dead_letter_handler() {
    let dead = Arc::new(AtomicUsize::new(0));

    let queue = Arc::new(InMemoryJobQueue::with_workers(2));
    // Register the dead-letter sink BEFORE starting workers.
    let dead_for_cb = dead.clone();
    queue
        .on_dead_letter(move |dl| {
            let counter = dead_for_cb.clone();
            async move {
                assert_eq!(dl.name, "doc:bad_payload");
                assert!(dl.error.contains("unprocessable"));
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
    queue.register::<BadPayload>().await;
    queue.start().await;

    queue.dispatch(&BadPayload).await.unwrap();

    wait_until(|| dead.load(Ordering::SeqCst) == 1).await;
    queue.shutdown().await;
    assert_eq!(
        dead.load(Ordering::SeqCst),
        1,
        "fatal job was dead-lettered"
    );
}
