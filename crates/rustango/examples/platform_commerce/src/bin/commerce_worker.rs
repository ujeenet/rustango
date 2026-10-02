//! Standalone worker: drains the job queue, serves no HTTP.
//!
//! Scaffolded by `manage make:worker CommerceWorker`, then filled in —
//! the template leaves the `register` calls as a comment on purpose,
//! because only the app knows its job types.
//!
//! **Filling them in is not optional.** A database-queue worker that
//! picks up a row whose `NAME` is not registered *in this process* logs
//! and returns without unlocking the row. The row is then stranded
//! until a `reclaim_stuck_jobs_pool` sweep frees it — whereupon it is
//! picked up and stranded again — and it never appears in
//! `pending_count()`. The queue looks empty and drains nothing.
//!
//! Run beside the web process, or as its own container. The soak does
//! the latter for Postgres and MySQL, which is the only way to get two
//! processes competing for the same rows: `rustango_jobs` is claimed
//! with three different per-dialect strategies, and a single-process
//! queue exercises none of that contention.

use std::sync::Arc;
use std::time::Duration;

use platform_commerce::commerce::jobs;
use rustango::jobs::{DatabaseJobQueue, JobQueue as _};
use rustango::sql::Pool;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let url = std::env::var("DATABASE_URL").map_err(|_| {
        "missing env var 'DATABASE_URL'. Set it in your shell, or copy '.env.example' to '.env'."
    })?;
    let pool = Pool::connect(&url).await?;

    // The handlers receive no pool of their own, so they look one up by
    // the slug carried in the payload.
    jobs::register_pool(jobs::SINGLE, pool.clone());

    // Tri-dialect despite the `Database` name: the table DDL and the
    // row-claim strategy are both chosen from the pool's dialect.
    DatabaseJobQueue::ensure_table_pool(&pool).await?;
    let queue = Arc::new(
        DatabaseJobQueue::with_workers_pool(pool.clone(), 4)
            .poll_interval(Duration::from_millis(250)),
    );

    // All four types the server can dispatch. See the module doc.
    jobs::register_all(&queue).await;
    queue
        .on_dead_letter(|dl| async move { jobs::log_dead_letter(None, &dl) })
        .await;

    // Rows whose worker died mid-job stay locked. Sweeping at boot
    // recovers anything a previous SIGKILL abandoned — the soak kills a
    // worker on purpose to check this path actually runs.
    match DatabaseJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(300)).await {
        Ok(n) if n > 0 => tracing::warn!(reclaimed = n, "recovered stuck jobs at boot"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "reclaim sweep failed"),
    }

    queue.start().await;
    tracing::info!(
        registered = ?jobs::registered_job_names(),
        "worker draining"
    );

    // SIGINT *and* SIGTERM. `tokio::signal::ctrl_c()` alone is
    // SIGINT-only, so under `docker stop` the drain below never runs and
    // in-flight jobs are lost with nothing logged (#1409).
    rustango::shutdown::shutdown_signal().await;

    tracing::info!("signal received, draining in-flight jobs");
    queue.shutdown().await;
    Ok(())
}
