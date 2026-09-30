//! #1843 — a panicking job does not kill its worker and counts as a run;
//! a long job is not run twice by a reclaim, and a worker that lost its
//! lease does not finish the row for the worker that holds it now.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "jobs-postgres",
    feature = "testkit"
))]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustango::core::SqlValue;
use rustango::jobs::pg::PgJobQueue;
use rustango::jobs::{Job, JobDeadLetter, JobError, JobQueue};
use rustango::sql::Pool;
use rustango::tri_dialect_test;
use serde::{Deserialize, Serialize};

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, "rustango_jobs").await;
    PgJobQueue::ensure_table_pool(pool)
        .await
        .expect("ensure rustango_jobs");
}

/// Per-token `(started, finished)` run counts; tokens keep the three
/// backends' arms apart when they share a process.
fn runs() -> &'static Mutex<HashMap<String, (u32, u32)>> {
    static R: OnceLock<Mutex<HashMap<String, (u32, u32)>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

fn start(token: &str) -> u32 {
    let mut r = runs().lock().unwrap();
    let e = r.entry(token.to_owned()).or_default();
    e.0 += 1;
    e.0
}

fn finish(token: &str) {
    runs()
        .lock()
        .unwrap()
        .entry(token.to_owned())
        .or_default()
        .1 += 1;
}

fn counts(token: &str) -> (u32, u32) {
    runs()
        .lock()
        .unwrap()
        .get(token)
        .copied()
        .unwrap_or_default()
}

fn token(pool: &Pool, name: &str) -> String {
    format!("{}:{name}", pool.dialect().name())
}

#[derive(Serialize, Deserialize)]
struct Boom {
    token: String,
}

#[async_trait::async_trait]
impl Job for Boom {
    const NAME: &'static str = "tri1843:boom";
    const MAX_ATTEMPTS: u32 = 1;
    async fn run(&self) -> Result<(), JobError> {
        start(&self.token);
        panic!("boom");
    }
}

#[derive(Serialize, Deserialize)]
struct Tick {
    token: String,
}

#[async_trait::async_trait]
impl Job for Tick {
    const NAME: &'static str = "tri1843:tick";
    async fn run(&self) -> Result<(), JobError> {
        start(&self.token);
        finish(&self.token);
        Ok(())
    }
}

/// Sleeps `first_ms` on its first run and `later_ms` after that.
#[derive(Serialize, Deserialize)]
struct Slow {
    token: String,
    first_ms: u64,
    later_ms: u64,
}

#[async_trait::async_trait]
impl Job for Slow {
    const NAME: &'static str = "tri1843:slow";
    async fn run(&self) -> Result<(), JobError> {
        let n = start(&self.token);
        let ms = if n == 1 { self.first_ms } else { self.later_ms };
        tokio::time::sleep(Duration::from_millis(ms)).await;
        finish(&self.token);
        Ok(())
    }
}

async fn queue(pool: &Pool, workers: usize, heartbeat: Duration) -> PgJobQueue {
    let q = PgJobQueue::with_workers_pool(pool.clone(), workers)
        .poll_interval(Duration::from_millis(20))
        .heartbeat_interval(heartbeat);
    q.register::<Boom>().await;
    q.register::<Tick>().await;
    q.register::<Slow>().await;
    q
}

async fn dead_letters(q: &PgJobQueue) -> Arc<Mutex<Vec<JobDeadLetter>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    q.on_dead_letter(move |dl| {
        s.lock().unwrap().push(dl);
        async {}
    })
    .await;
    seen
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn job_rows(pool: &Pool) -> i64 {
    let rows: Vec<(i64,)> =
        rustango::sql::raw_query_pool("SELECT COUNT(*) FROM rustango_jobs", Vec::new(), pool)
            .await
            .expect("count");
    rows[0].0
}

async fn a_panicking_job_keeps_the_worker(pool: &Pool) {
    let (boom, tick) = (token(pool, "boom"), token(pool, "tick"));
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    let dead = dead_letters(&q).await;
    q.dispatch(&Boom {
        token: boom.clone(),
    })
    .await
    .unwrap();
    q.dispatch(&Tick {
        token: tick.clone(),
    })
    .await
    .unwrap();
    q.start().await;

    wait_for("the next job on the same worker", || counts(&tick).1 == 1).await;
    wait_for("the dead letter", || !dead.lock().unwrap().is_empty()).await;
    q.shutdown().await;
    assert_eq!(counts(&boom).0, 1, "the panic counted as its one run");
    let dl = dead.lock().unwrap();
    assert!(dl[0].error.contains("panicked"), "{:?}", dl[0].error);
    assert_eq!(job_rows(pool).await, 0);
}

async fn attempt_counts_at_pickup(pool: &Pool) {
    let slow = token(pool, "pickup");
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    q.dispatch(&Slow {
        token: slow.clone(),
        first_ms: 1500,
        later_ms: 0,
    })
    .await
    .unwrap();
    q.start().await;
    wait_for("the run to start", || counts(&slow).0 == 1).await;
    let rows: Vec<(i32,)> =
        rustango::sql::raw_query_pool("SELECT attempt FROM rustango_jobs", Vec::new(), pool)
            .await
            .expect("attempt");
    assert_eq!(rows[0].0, 1, "a crash mid-run still counts the run");
    q.shutdown().await;
}

async fn a_row_out_of_attempts_is_dead_lettered_not_run(pool: &Pool) {
    // What a crashed worker leaves behind once reclaimed: every attempt spent.
    let boom = token(pool, "spent");
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    let dead = dead_letters(&q).await;
    q.dispatch(&Boom {
        token: boom.clone(),
    })
    .await
    .unwrap();
    rustango::sql::raw_execute_pool(pool, "UPDATE rustango_jobs SET attempt = 1", Vec::new())
        .await
        .expect("spend the attempts");
    q.start().await;
    wait_for("the dead letter", || !dead.lock().unwrap().is_empty()).await;
    q.shutdown().await;
    assert_eq!(counts(&boom).0, 0, "not run again");
    assert_eq!(job_rows(pool).await, 0);
}

async fn a_heartbeat_keeps_a_long_job_leased(pool: &Pool) {
    let slow = token(pool, "heartbeat");
    let q = queue(pool, 2, Duration::from_millis(100)).await;
    q.dispatch(&Slow {
        token: slow.clone(),
        first_ms: 1500,
        later_ms: 1500,
    })
    .await
    .unwrap();
    q.start().await;
    wait_for("the run to start", || counts(&slow).0 == 1).await;
    // A reclaim sweep far shorter than the job, but longer than the heartbeat.
    while counts(&slow).1 == 0 {
        PgJobQueue::reclaim_stuck_jobs_pool(pool, Duration::from_millis(500))
            .await
            .expect("reclaim");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    q.shutdown().await;
    assert_eq!(counts(&slow).0, 1, "ran once");
}

async fn a_lost_lease_does_not_finish_the_new_holders_row(pool: &Pool) {
    let slow = token(pool, "lease");
    let q = queue(pool, 2, Duration::from_secs(60)).await;
    q.dispatch(&Slow {
        token: slow.clone(),
        first_ms: 600,
        later_ms: 4000,
    })
    .await
    .unwrap();
    q.start().await;
    wait_for("the first run", || counts(&slow).0 == 1).await;
    let reclaimed = PgJobQueue::reclaim_stuck_jobs_pool(pool, Duration::ZERO)
        .await
        .expect("reclaim");
    assert_eq!(reclaimed, 1);
    wait_for("the second run", || counts(&slow).0 == 2).await;
    wait_for("the first run to finish", || counts(&slow).1 == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let d = pool.dialect();
    let sql = format!(
        "SELECT COUNT(*) FROM rustango_jobs WHERE locked_by IS NOT NULL AND attempt = {}",
        d.placeholder(1)
    );
    let rows: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, vec![SqlValue::I32(2)], pool)
        .await
        .expect("row");
    assert_eq!(rows[0].0, 1, "still leased to the second run");
    q.shutdown().await;
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        a_panicking_job_keeps_the_worker,
        attempt_counts_at_pickup,
        a_row_out_of_attempts_is_dead_lettered_not_run,
        a_heartbeat_keeps_a_long_job_leased,
        a_lost_lease_does_not_finish_the_new_holders_row,
    ],
}
