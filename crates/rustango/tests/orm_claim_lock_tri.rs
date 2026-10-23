//! "Claim the next job" with `select_for_update` on every backend. On SQLite
//! the transaction must take the write lock at `BEGIN`, or the second
//! worker's UPDATE fails with SQLITE_BUSY.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{ExecError, Pool};
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

/// Two workers racing over two jobs each claim one, every round.
async fn racing_workers_claim_distinct_jobs(pool: &Pool) {
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
            async move {
                barrier.wait().await;
                claim(pool, worker).await
            }
        };
        let (a, b) = tokio::join!(run(1), run(2));
        let a = a.unwrap_or_else(|e| panic!("round {round}: worker 1: {e}"));
        let b = b.unwrap_or_else(|e| panic!("round {round}: worker 2: {e}"));
        assert!(a.is_some() && b.is_some() && a != b, "round {round}: {a:?} {b:?}");
    }
}

tri_dialect_test! {
    model: Job,
    sqlite: file,
    scenarios: [racing_workers_claim_distinct_jobs],
}
