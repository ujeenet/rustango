//! Auto-scaffolded by `manage make:worker CommerceWorker`.
//!
//! A standalone worker process: it drains the job queue and serves no
//! HTTP. Run it alongside the web process, or as its own container.
//!
//! Put this at `src/bin/commerce_worker.rs` and run it with `cargo run --bin commerce_worker`.

use std::sync::Arc;
use std::time::Duration;

use rustango::jobs::{DatabaseJobQueue, JobQueue};
use rustango::sql::Pool;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::logging::setup();
    // `std::env::var(..)?` would surface as the bare word `NotPresent`,
    // which tells an operator nothing about which variable or why.
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        "missing env var 'DATABASE_URL'. Set it in your shell, or copy \
         '.env.example' to '.env'."
    })?;
    let pool = Pool::connect(&url).await?;

    // The queue is tri-dialect despite the `Database` name — the table
    // DDL and the row-pickup strategy are chosen from the pool's dialect.
    DatabaseJobQueue::ensure_table_pool(&pool).await?;
    let queue = Arc::new(DatabaseJobQueue::with_workers_pool(pool.clone(), 4));

    // Register EVERY job type this queue might see, not just the ones
    // this process dispatches. A worker that picks up a row whose name
    // is unregistered here logs and returns *without unlocking it* — the
    // row is then stranded until a `reclaim_stuck_jobs_pool` sweep, and
    // it does not show up in `pending_count()`.
    //
    //   queue.register::<crate::jobs::WelcomeEmail>().await;

    queue.start().await;
    tracing::info!("commerce_worker: draining jobs");

    // SIGINT *and* SIGTERM. `tokio::signal::ctrl_c()` alone is
    // SIGINT-only, so under `docker stop` the drain below never runs.
    rustango::shutdown::shutdown_signal().await;

    tracing::info!("commerce_worker: signal received, draining in-flight jobs");
    queue.shutdown().await;

    // Rows whose worker died mid-job stay locked. Nothing sweeps them
    // for you; run this on a scheduler, or at boot as done here.
    let _ = DatabaseJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(300)).await;
    Ok(())
}
