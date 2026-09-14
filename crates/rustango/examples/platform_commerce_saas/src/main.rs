//! Multi-tenant commerce platform — the soak-test application.
//!
//! Twin of `platform_commerce`, which is this app without `.tenancy()`.
//! Generated with:
//!
//! ```text
//! cargo rustango new platform_commerce_saas --template tenant \
//!     --backend postgres --features cache-redis --rustango-path ../..
//! ```
//!
//! Every backend feature is present in `Cargo.toml`; `--backend` only
//! chose which is `default`. Postgres tenants use **schema mode**;
//! MySQL and SQLite must use **database mode**, because `SET
//! search_path` has no equivalent there.

// `commerce` comes from the library target (src/lib.rs) so the
// worker binary can reach the same job types and the same tenant
// supervisor this binary uses. `urls`/`views` are server-only.
use platform_commerce_saas::commerce;

mod urls;
mod views;

use std::sync::Arc;

use rustango::sql::Pool;
use rustango::tenancy::{DefaultTenantDb, RouteConfig, TenantPools};

fn fail_ratio_pct() -> u8 {
    std::env::var("SOAK_FAIL_RATIO_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

/// Is this process about to serve HTTP, or run a CLI verb?
///
/// `Cli::run()` dispatches on argv: no args (or `runserver`) serves,
/// anything else is a management command. The supervisor below queries
/// `rustango_orgs`, so booting it unconditionally makes **every** verb
/// need tables that `migrate` has not created yet — including `migrate`
/// itself. That is a chicken-and-egg that makes a fresh project
/// impossible to bootstrap, and it is a mistake worth not repeating:
/// keep start-up work that touches the database behind this check.
fn will_serve() -> bool {
    match std::env::args().nth(1) {
        None => true,
        Some(a) => a == "runserver" || a == "run-server",
    }
}

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let url = std::env::var("DATABASE_URL").map_err(|_| {
        "missing env var 'DATABASE_URL'. Set it in your shell, or copy '.env.example' to '.env'."
    })?;

    // Management verbs (`migrate`, `create-tenant`, …) take the plain
    // path: no queues, no pools, nothing that reads a table that may not
    // exist yet.
    if !will_serve() {
        return rustango::manage::Cli::new()
            .tenancy()
            .routes(RouteConfig::legacy())
            .run()
            .await;
    }

    // Two handles on the same database, for two different jobs.
    // `TenantPools` needs the *typed* pool — `DefaultTenantDb` resolves
    // to whichever backend this build selected. The erased `Pool` is
    // what the ORM takes.
    let typed = rustango::sql::sqlx::Pool::<DefaultTenantDb>::connect(&url).await?;
    let registry = Pool::from(typed.clone());

    // One queue per active tenant. A tenant provisioned after this
    // point gets workers from the refresh loop rather than at restart
    // (#1223).
    //
    // The supervisor's errors are `Send + Sync` because they cross an
    // await inside a spawned task; `#[rustango::main]` returns the
    // plain `Box<dyn Error>`, so they are flattened here.
    let pools = Arc::new(TenantPools::<DefaultTenantDb>::new(typed));
    let queues = commerce::supervisor::boot(&registry, &pools)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
    let _refresh = commerce::supervisor::spawn_refresh(
        Arc::clone(&queues),
        registry.clone(),
        Arc::clone(&pools),
    );

    let drain = Arc::clone(&queues);
    rustango::manage::Cli::new()
        .tenancy()
        // Moves the operator console and the tenant admin off `/admin`,
        // `/login`, `/logout`, `/change-password`, `/_static` and
        // `/_brand` — all six of which a storefront wants. Under
        // `legacy()` the console's `/__admin` coincides with the prefix
        // the framework claims unconditionally, so this costs one
        // namespace rather than two.
        .routes(RouteConfig::legacy())
        .api(urls::api(Arc::clone(&queues), fail_ratio_pct()))
        .with_health()
        // #1409 — the drain belongs here, not after `run()`. Before
        // 0.57.5 the tenancy server waited on `ctrl_c()`, which is
        // SIGINT-only, so under `docker stop` (SIGTERM) this never ran.
        .on_shutdown(move || async move {
            tracing::info!("draining every tenant's job queue before exit");
            commerce::supervisor::shutdown_all(&drain).await;
        })
        .run()
        .await
}
