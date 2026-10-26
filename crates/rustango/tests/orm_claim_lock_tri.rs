//! "Claim the next job" with `select_for_update` on every backend. On SQLite
//! the transaction must take the write lock at `BEGIN`, or the second
//! worker's UPDATE fails with SQLITE_BUSY.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{ExecError, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "ocl_job", app = "ocl")]
pub struct Job {
    #[rustango(primary_key)]
    pub id: i64,
    pub worker: Option<i64>,
}

/// Claim the lowest free job for `worker`, or `None` when all are taken.
async fn claim(pool: &Pool, worker: i64) -> Result<Option<i64>, ExecError> {
    let mut tx = rustango::sql::write_transaction(pool).await?;
    let free: Vec<Job> = Job::objects()
        .filter("worker__isnull", true)
        .order_by(&[("id", false)])
        .limit(1)
        .select_for_update()
        .skip_locked()
        .silent_on_sqlite()
        .fetch_on(&mut tx)
        .await?;
    let Some(job) = free.first() else {
        tx.commit().await?;
        return Ok(None);
    };
    Job::objects()
        .filter("id", job.id)
        .update()
        .set("worker", worker)
        .execute_on(&mut tx)
        .await?;
    tx.commit().await?;
    Ok(Some(job.id))
}

/// [`claim`] inside `atomic_immediate`, which takes the write lock at `BEGIN` too.
async fn claim_atomic(pool: &Pool, worker: i64) -> Result<Option<i64>, ExecError> {
    rustango::sql::atomic_immediate(pool, move |tx| {
        Box::pin(async move {
            let mut guard = tx.lock().await?;
            let free: Vec<Job> = Job::objects()
                .filter("worker__isnull", true)
                .order_by(&[("id", false)])
                .limit(1)
                .select_for_update()
                .skip_locked()
                .silent_on_sqlite()
                .fetch_on(&mut *guard)
                .await?;
            let Some(job) = free.first() else {
                return Ok(None);
            };
            Job::objects()
                .filter("id", job.id)
                .update()
                .set("worker", worker)
                .execute_on(&mut *guard)
                .await?;
            Ok(Some(job.id))
        })
    })
    .await
}

/// A default `atomic` stays deferred: a pool write inside it, on another
/// connection, does not wait for the block's lock.
async fn pool_write_inside_a_default_atomic(pool: &Pool) {
    Job::objects().delete_on(pool).await.expect("clear");
    let p = pool.clone();
    rustango::atomic!(pool, |tx| {
        let seen = Job::objects().fetch_on(&mut *tx.lock().await?).await?;
        assert!(seen.is_empty());
        Job {
            id: 1,
            worker: None,
        }
        .insert_pool(&p)
        .await?;
        Ok(())
    })
    .await
    .expect("a pool write inside atomic");
    let jobs = Job::objects().fetch(pool).await.expect("fetch");
    assert_eq!(jobs.len(), 1);
}

/// Two workers racing over two jobs each claim one, every round.
async fn racing_workers_claim_distinct_jobs(pool: &Pool) {
    race(pool, |pool, worker| Box::pin(claim(pool, worker))).await;
}

/// The same race in `atomic_immediate` blocks.
async fn racing_atomic_workers_claim_distinct_jobs(pool: &Pool) {
    race(pool, |pool, worker| Box::pin(claim_atomic(pool, worker))).await;
}

type Claim<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<i64>, ExecError>> + 'a>>;

async fn race(pool: &Pool, claim: impl for<'a> Fn(&'a Pool, i64) -> Claim<'a>) {
    for round in 0..20 {
        Job::objects().delete_on(pool).await.expect("clear");
        for id in [1, 2] {
            Job { id, worker: None }
                .insert_pool(pool)
                .await
                .expect("job");
        }
        let barrier = tokio::sync::Barrier::new(2);
        let run = |worker: i64| {
            let barrier = &barrier;
            let claim = &claim;
            async move {
                barrier.wait().await;
                claim(pool, worker).await
            }
        };
        let (a, b) = tokio::join!(run(1), run(2));
        let a = a.unwrap_or_else(|e| panic!("round {round}: worker 1: {e}"));
        let b = b.unwrap_or_else(|e| panic!("round {round}: worker 2: {e}"));
        assert!(
            a.is_some() && b.is_some() && a != b,
            "round {round}: {a:?} {b:?}"
        );
    }
}

tri_dialect_test! {
    model: Job,
    sqlite: file,
    scenarios: [
        racing_workers_claim_distinct_jobs,
        racing_atomic_workers_claim_distinct_jobs,
        pool_write_inside_a_default_atomic,
    ],
}
