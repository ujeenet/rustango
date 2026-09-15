//! Standalone worker, multi-tenant: drains every tenant's queue.
//!
//! Scaffolded by `manage make:worker CommerceWorker`, then filled in.
//! The single-tenant twin builds one queue; this one builds one per
//! tenant, because `Job::run(&self)` receives no tenant context and
//! isolation comes entirely from the pool its queue was built on.
//!
//! It reuses `commerce::supervisor` rather than reimplementing the
//! fan-out, so the worker and the server cannot drift about which
//! tenants have queues or which job types are registered on them — the
//! drift that strands rows invisibly.
//!
//! Not wired into the soak's compose fleet today: the SaaS instances
//! run their workers in-process. It exists because the shape is the
//! interesting part, and because a reader who wants a separate worker
//! tier needs to see how the per-tenant fan-out survives the split.

use std::sync::Arc;

use platform_commerce_saas::commerce::supervisor;
use rustango::sql::Pool;
use rustango::tenancy::{DefaultTenantDb, TenantPools};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let url = std::env::var("DATABASE_URL").map_err(|_| {
        "missing env var 'DATABASE_URL'. Set it in your shell, or copy '.env.example' to '.env'."
    })?;

    // `TenantPools` needs the typed pool; `DefaultTenantDb` is whichever
    // backend this build selected. A tenancy build's registry dialect is
    // fixed at compile time, which is why the soak ships one SaaS image
    // per dialect.
    let typed = rustango::sql::sqlx::Pool::<DefaultTenantDb>::connect(&url).await?;
    let registry = Pool::from(typed.clone());
    // The same sizing the server uses (#1456). Read from the library so
    // the two cannot disagree: a worker tier sized differently from the
    // web tier is how a fleet exhausts a database while every process
    // looks correctly configured on its own.
    let pools = Arc::new(
        TenantPools::<DefaultTenantDb>::new(typed).config(supervisor::pool_config_from_env()),
    );

    let queues = supervisor::boot(&registry, &pools)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
    // Tenants provisioned after boot get workers from here rather than
    // at the next restart (#1223).
    let _refresh =
        supervisor::spawn_refresh(Arc::clone(&queues), registry.clone(), Arc::clone(&pools));

    tracing::info!(tenants = queues.slugs().len(), "worker draining");

    // SIGINT *and* SIGTERM — `docker stop` sends the latter (#1409).
    rustango::shutdown::shutdown_signal().await;

    tracing::info!("signal received, draining every tenant's queue");
    supervisor::shutdown_all(&queues).await;
    Ok(())
}
