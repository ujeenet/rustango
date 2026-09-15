//! Single-tenant commerce platform — the soak-test application.
//!
//! Twin of `platform_commerce_saas`, which is the same app with
//! `.tenancy()`. Generated with:
//!
//! ```text
//! cargo rustango new platform_commerce --template fullstack \
//!     --backend postgres --features cache-redis --rustango-path ../..
//! ```
//!
//! Every backend feature is present in `Cargo.toml`; `--backend` only
//! chose which is `default`. Run on another dialect with
//! `cargo run --no-default-features --features sqlite`.

// `commerce` comes from the library target (src/lib.rs) so the
// worker binary can reach the same job types this binary
// dispatches. `urls`/`views` are server-only.
use platform_commerce::commerce;

mod urls;
mod views;

use std::sync::Arc;
use std::time::Duration;

use rustango::jobs::{DatabaseJobQueue, JobQueue as _};
use rustango::sql::Pool;

/// Percentage of payment captures that fail every attempt. The harness
/// predicts the dead-letter count from this, so it must be stable for
/// the length of a run.
fn fail_ratio_pct() -> u8 {
    std::env::var("SOAK_FAIL_RATIO_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

/// Workers in this process, or in a separate container?
///
/// The SQLite topology runs both here: two containers sharing one
/// SQLite file would need POSIX advisory locks plus WAL shared memory
/// across the container boundary, which is unreliable on a bind mount.
/// Postgres and MySQL use a dedicated worker container instead.
fn inline_workers() -> bool {
    std::env::var("SOAK_INLINE_WORKERS").is_ok_and(|v| v == "1")
}

/// Is this process about to serve HTTP, or run a CLI verb?
///
/// `Cli::run()` dispatches on argv: no args (or `runserver`) serves,
/// anything else is a management command. Everything below this check
/// touches the database, so doing it unconditionally makes verbs that
/// need no database — `version`, `showmodels`, `make:*` — fail without
/// one, and makes `migrate` need tables it is about to create.
///
/// Keep start-up work that touches the database behind this check. The
/// framework's own `Cli` does its connecting inside dispatch for the
/// same reason.
fn will_serve() -> bool {
    match std::env::args().nth(1) {
        None => true,
        Some(a) => a == "runserver" || a == "run-server",
    }
}

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    if !will_serve() {
        return rustango::manage::Cli::new().run().await;
    }

    let url = std::env::var("DATABASE_URL").map_err(|_| {
        "missing env var 'DATABASE_URL'. Set it in your shell, or copy '.env.example' to '.env'."
    })?;
    let pool = Pool::connect(&url).await?;

    // One pool, filed under the single-tenant slug, so the job handlers
    // — which receive no pool of their own — can find it.
    commerce::jobs::register_pool(commerce::jobs::SINGLE, pool.clone());

    DatabaseJobQueue::ensure_table_pool(&pool).await?;
    let queue = Arc::new(
        DatabaseJobQueue::with_workers_pool(pool.clone(), 4)
            .poll_interval(Duration::from_millis(250)),
    );
    commerce::jobs::register_all(&queue).await;
    // Without this the in-process queue falls through to the framework's
    // generic "no callback configured" log — which is what the SQLite
    // instance did, 98 times, while every other instance attributed its
    // dead letters properly.
    queue
        .on_dead_letter(|dl| async move { commerce::jobs::log_dead_letter(None, &dl) })
        .await;

    tracing::info!(
        dialect = pool.dialect().name(),
        fail_ratio_pct = fail_ratio_pct(),
        inline_workers = inline_workers(),
        "platform_commerce starting"
    );
    tracing::debug!(registered = ?commerce::jobs::registered_job_names(), "job types");

    if inline_workers() {
        queue.start().await;
        tracing::info!(
            workers = 4,
            "workers running in-process (SOAK_INLINE_WORKERS=1)"
        );
    } else {
        tracing::info!("dispatch-only; a separate worker container drains the queue");
    }

    let drain = Arc::clone(&queue);
    rustango::manage::Cli::new()
        .api(urls::api(
            pool.clone(),
            Arc::clone(&queue),
            fail_ratio_pct(),
        ))
        .with_health()
        // #1409. This is the only correct place for the drain: before
        // 0.57.5 `Cli::run` installed no graceful shutdown at all, so
        // SIGTERM killed the process and anything written after `run()`
        // never executed. `docker stop` sends SIGTERM.
        .on_shutdown(move || async move {
            tracing::info!("draining job queue before exit");
            drain.shutdown().await;
        })
        .run()
        .await
}
