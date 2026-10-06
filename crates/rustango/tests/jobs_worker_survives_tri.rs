//! #1843 — a panicking job does not kill its worker and counts as a run;
//! a long job is not run twice by a reclaim, and a worker that lost its
//! lease does not finish the row for the worker that holds it now.
//! #1677: a queue restarts after `shutdown`. #1229: a job runs with
//! its enqueuer's audit source and timezone.

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

/// Fails fatally on its first run (after `first_ms`), succeeds after.
#[derive(Serialize, Deserialize)]
struct FailFirst {
    token: String,
    first_ms: u64,
}

#[async_trait::async_trait]
impl Job for FailFirst {
    const NAME: &'static str = "tri1843:fail_first";
    async fn run(&self) -> Result<(), JobError> {
        let n = start(&self.token);
        if n == 1 {
            tokio::time::sleep(Duration::from_millis(self.first_ms)).await;
        }
        finish(&self.token);
        if n == 1 {
            Err(JobError::Fatal("first run".into()))
        } else {
            Ok(())
        }
    }
}

/// Never registered: no worker here can run it.
#[derive(Serialize, Deserialize)]
struct Orphan;

#[async_trait::async_trait]
impl Job for Orphan {
    const NAME: &'static str = "tri1843:orphan";
    async fn run(&self) -> Result<(), JobError> {
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
    q.register::<FailFirst>().await;
    q
}

/// `(attempt, locked)` of the only row.
async fn the_row(pool: &Pool) -> (i32, bool) {
    let rows: Vec<(i32, i64)> = rustango::sql::raw_query_pool(
        "SELECT attempt, COUNT(locked_by) FROM rustango_jobs GROUP BY attempt",
        Vec::new(),
        pool,
    )
    .await
    .expect("row");
    (rows[0].0, rows[0].1 == 1)
}

async fn wait_locked(pool: &Pool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !the_row(pool).await.1 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the pickup"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

async fn a_job_without_a_handler_spends_no_attempts(pool: &Pool) {
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    q.dispatch(&Orphan).await.unwrap();
    q.start().await;
    for _ in 0..2 {
        wait_locked(pool).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(the_row(pool).await, (0, true), "picked, not charged");
        PgJobQueue::reclaim_stuck_jobs_pool(pool, Duration::ZERO)
            .await
            .expect("reclaim");
    }
    q.shutdown().await;
}

async fn a_lost_lease_fires_no_dead_letter(pool: &Pool) {
    let tok = token(pool, "lost_dl");
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    let dead = dead_letters(&q).await;
    q.dispatch(&FailFirst {
        token: tok.clone(),
        first_ms: 600,
    })
    .await
    .unwrap();
    q.start().await;
    wait_for("the first run", || counts(&tok).0 == 1).await;
    PgJobQueue::reclaim_stuck_jobs_pool(pool, Duration::ZERO)
        .await
        .expect("reclaim");
    wait_for("the rerun to finish", || counts(&tok).1 == 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    q.shutdown().await;
    assert!(
        dead.lock().unwrap().is_empty(),
        "the lost run is not dead-lettered"
    );
    assert_eq!(job_rows(pool).await, 0, "the rerun finished it");
}

async fn shutdown_releases_an_aborted_job(pool: &Pool) {
    let tok = token(pool, "abort");
    let q = queue(pool, 1, Duration::from_secs(60))
        .await
        .shutdown_grace(Duration::from_millis(200));
    q.dispatch(&Slow {
        token: tok.clone(),
        first_ms: 30_000,
        later_ms: 0,
    })
    .await
    .unwrap();
    q.start().await;
    wait_for("the run to start", || counts(&tok).0 == 1).await;
    let began = Instant::now();
    q.shutdown().await;
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "shutdown_grace bounds the wait: {:?}",
        began.elapsed()
    );
    assert_eq!(
        the_row(pool).await,
        (1, false),
        "unlocked for the next worker"
    );
}

/// #1677: `start` after `shutdown` runs jobs again; the stop flag was never reset.
async fn start_after_shutdown_runs_jobs(pool: &Pool) {
    let tick = token(pool, "restart");
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    q.start().await;
    q.shutdown().await;
    q.start().await;
    q.dispatch(&Tick {
        token: tick.clone(),
    })
    .await
    .unwrap();
    wait_for("the job after a restart", || counts(&tick).1 == 1).await;
    q.shutdown().await;
}

/// A queue dropped without `shutdown` stops its workers instead of
/// spinning on the closed stop signal.
async fn a_dropped_queue_stops_its_workers(pool: &Pool) {
    let tick = token(pool, "dropped");
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    q.start().await;
    drop(q);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let idle = queue(pool, 0, Duration::from_secs(60)).await;
    idle.dispatch(&Tick {
        token: tick.clone(),
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(counts(&tick), (0, 0), "no worker left to run it");
    assert_eq!(the_row(pool).await, (0, false), "nor to lock it");
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "jobs1229_note", app = "jobs1229", audit(track = "title"))]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
}

/// Pool and seen UTC offset per token: a payload cannot carry a pool.
fn note_pools() -> &'static Mutex<HashMap<String, (Pool, Option<i32>)>> {
    static P: OnceLock<Mutex<HashMap<String, (Pool, Option<i32>)>>> = OnceLock::new();
    P.get_or_init(Mutex::default)
}

/// Writes one audited note titled `token`.
#[derive(Serialize, Deserialize)]
struct WriteNote {
    token: String,
}

#[async_trait::async_trait]
impl Job for WriteNote {
    const NAME: &'static str = "tri1229:write_note";
    async fn run(&self) -> Result<(), JobError> {
        let offset = rustango::i18n::timezone::current_offset().local_minus_utc();
        let pool = {
            let mut p = note_pools().lock().unwrap();
            let e = p.get_mut(&self.token).expect("pool");
            e.1 = Some(offset);
            e.0.clone()
        };
        let mut note = Note {
            id: rustango::sql::Auto::default(),
            title: self.token.clone(),
        };
        note.insert_pool(&pool)
            .await
            .map_err(|e| JobError::Fatal(e.to_string()))
    }
}

async fn note_setup(pool: &Pool) {
    use rustango::audit::{self, AuditLog};
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    AuditLog::delete_where("entity_table", "jobs1229_note", pool)
        .await
        .expect("clear audit rows");
}

/// Dispatch a `WriteNote` as `user` in UTC+3, or with no scope at all.
async fn dispatch_note(q: &PgJobQueue, pool: &Pool, tok: &str, user: Option<&str>) {
    use rustango::audit::{with_source, AuditSource};
    note_pools()
        .lock()
        .unwrap()
        .insert(tok.to_owned(), (pool.clone(), None));
    let job = WriteNote {
        token: tok.to_owned(),
    };
    match user {
        Some(id) => {
            let three_h = chrono::FixedOffset::east_opt(3 * 3600).unwrap();
            let source = AuditSource::User { id: id.into() };
            with_source(source, async {
                rustango::i18n::timezone::with_offset(three_h, q.dispatch(&job)).await
            })
            .await
        }
        None => q.dispatch(&job).await,
    }
    .unwrap();
}

/// The audit source and UTC offset the job for `tok` ran with.
async fn ran_as(pool: &Pool, tok: &str) -> (String, i32) {
    use rustango::sql::FetcherPool as _;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {tok}");
        let notes = Note::objects()
            .filter("title", tok)
            .fetch(pool)
            .await
            .expect("fetch");
        if let Some(note) = notes.first() {
            let pk = note.id.get().expect("pk").to_string();
            let rows = rustango::audit::fetch_for_entity_pool(pool, "jobs1229_note", &pk)
                .await
                .expect("audit rows");
            if let Some(e) = rows.first() {
                let offset = note_pools().lock().unwrap()[tok].1.expect("offset");
                return (e.source.clone(), offset);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// #1229 — a `PgJobQueue` job runs with its enqueuer's audit source and
/// timezone; one enqueued outside any scope stays `system` in UTC.
async fn a_database_job_runs_as_its_enqueuer(pool: &Pool) {
    note_setup(pool).await;
    let (by_user, by_nobody) = (token(pool, "ctx_user"), token(pool, "ctx_nobody"));
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    q.register::<WriteNote>().await;
    q.start().await;
    dispatch_note(&q, pool, &by_user, Some("42")).await;
    dispatch_note(&q, pool, &by_nobody, None).await;
    let user = ran_as(pool, &by_user).await;
    let nobody = ran_as(pool, &by_nobody).await;
    q.shutdown().await;
    assert_eq!(user, ("user:42".to_owned(), 3 * 3600));
    assert_eq!(nobody, ("system".to_owned(), 0));
}

/// #1229 — a table without `context` still runs jobs, as `system`;
/// `ensure_table_pool` adds the column and the context then crosses.
async fn an_older_table_gets_the_column_from_ensure_table(pool: &Pool) {
    note_setup(pool).await;
    rustango::sql::raw_execute_pool(
        pool,
        "ALTER TABLE rustango_jobs DROP COLUMN context",
        Vec::new(),
    )
    .await
    .expect("drop the column");
    let (before, after) = (token(pool, "ctx_old"), token(pool, "ctx_ensured"));

    let q = queue(pool, 1, Duration::from_secs(60)).await;
    q.register::<WriteNote>().await;
    q.start().await;
    dispatch_note(&q, pool, &before, Some("7")).await;
    let old = ran_as(pool, &before).await;
    q.shutdown().await;
    assert_eq!(old.0, "system", "no column, no context");

    PgJobQueue::ensure_table_pool(pool).await.expect("ensure");
    let q = queue(pool, 1, Duration::from_secs(60)).await;
    q.register::<WriteNote>().await;
    q.start().await;
    dispatch_note(&q, pool, &after, Some("7")).await;
    let ensured = ran_as(pool, &after).await;
    q.shutdown().await;
    assert_eq!(ensured.0, "user:7");
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        a_database_job_runs_as_its_enqueuer,
        an_older_table_gets_the_column_from_ensure_table,
        a_panicking_job_keeps_the_worker,
        attempt_counts_at_pickup,
        a_row_out_of_attempts_is_dead_lettered_not_run,
        a_heartbeat_keeps_a_long_job_leased,
        a_lost_lease_does_not_finish_the_new_holders_row,
        a_job_without_a_handler_spends_no_attempts,
        a_lost_lease_fires_no_dead_letter,
        shutdown_releases_an_aborted_job,
        start_after_shutdown_runs_jobs,
        a_dropped_queue_stops_its_workers,
    ],
}
