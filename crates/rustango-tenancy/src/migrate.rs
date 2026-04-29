//! Scoped migration runners — registry vs tenant.
//!
//! Migration files carry a `scope: registry | tenant` field
//! ([`rustango::migrate::MigrationScope`]); slice 1 added the field,
//! slice 3 (this module) wires it into routing.
//!
//! ## Registry migrations
//!
//! [`migrate_registry`] applies migrations marked
//! `scope = "registry"` to the registry DB. Tenant-scoped migrations
//! are skipped. Only the registry pool is touched. Idempotent +
//! ledger-tracked through the standard `__rustango_migrations__`
//! table in the registry's `public` schema.
//!
//! ## Tenant migrations
//!
//! [`migrate_tenants`] walks every active org from the registry,
//! resolves its pool via [`TenantPools`], and applies migrations
//! marked `scope = "tenant"` to that tenant's storage:
//!
//! * **Schema mode** — the runner spins up an *ephemeral* PgPool
//!   bound to the registry URL with an `after_connect` hook that
//!   issues `SET search_path TO <schema>, public` on every fresh
//!   connection. The migration runner from `rustango-migrate` runs
//!   unchanged against this pool; its ledger queries
//!   (`__rustango_migrations__`) resolve to `<schema>.__rustango_migrations__`
//!   thanks to search_path. Each tenant gets its own ledger row set.
//!   The ephemeral pool is dropped after the tenant finishes; we
//!   do not reuse it as the runtime tenant pool because runtime
//!   uses the shared registry pool with per-checkout `SET`
//!   (whereas migration wants connection-level `SET` for transaction
//!   safety).
//!
//! * **Database mode** — the runner uses the tenant's dedicated
//!   pool (built lazily through [`TenantPools::pool_for_org`]). The
//!   ledger lives at `public.__rustango_migrations__` in the tenant's
//!   own database — single-schema, no `search_path` dance.
//!
//! ## Failure isolation
//!
//! [`migrate_tenants`] does **not** abort the whole batch when one
//! tenant fails. Each tenant's outcome (applied migrations, secrets
//! errors, SQL errors) lands in [`TenantMigrationReport`]. The caller
//! decides what to surface — an operator dashboard / log digest /
//! blocking error in CI. The registry connection URL is currently
//! reconstructed from the registry pool; a future hook could let
//! callers supply it explicitly when their connection-string layout
//! demands.

use rustango::core::Column as _;
use rustango::migrate::{self, Migration, MigrationScope};
use rustango::sql::sqlx::postgres::PgPoolOptions;
use rustango::sql::sqlx::PgPool;
use rustango::sql::Fetcher;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use crate::error::TenancyError;
use crate::org::{Org, StorageMode};
use crate::pools::TenantPools;

/// Outcome of [`migrate_tenants`].
#[derive(Debug, Default)]
pub struct TenantMigrationReport {
    /// One entry per tenant, in the order they were processed.
    pub tenants: Vec<TenantMigrationOutcome>,
}

impl TenantMigrationReport {
    /// `true` when every tenant migrated cleanly.
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.tenants.iter().all(|t| t.error.is_none())
    }

    /// Number of tenants that errored.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.tenants.iter().filter(|t| t.error.is_some()).count()
    }
}

/// Per-tenant migration outcome.
#[derive(Debug)]
pub struct TenantMigrationOutcome {
    pub slug: String,
    /// Migrations newly applied to this tenant.
    pub applied: Vec<Migration>,
    /// `Some(_)` if the tenant errored; the rest of the batch
    /// continues regardless. The operator dashboard / CI step
    /// should surface this.
    pub error: Option<TenancyError>,
}

/// Apply registry-scoped pending migrations to the registry DB.
///
/// Only migrations whose `scope == Registry` run. Tenant-scoped
/// migrations are silently skipped here — they're for
/// [`migrate_tenants`].
///
/// # Errors
/// As [`rustango_migrate::migrate`].
pub async fn migrate_registry(
    pools: &TenantPools,
    dir: &Path,
) -> Result<Vec<Migration>, TenancyError> {
    info!(target: "rustango_tenancy", "applying registry-scoped migrations");
    let scoped_dir = scoped_subset(dir, MigrationScope::Registry).await?;
    let applied = match scoped_dir {
        ScopedDir::Owned(temp) => {
            let result = migrate::migrate(pools.registry(), temp.path()).await?;
            drop(temp);
            result
        }
        ScopedDir::Original => migrate::migrate(pools.registry(), dir).await?,
    };
    info!(
        target: "rustango_tenancy",
        applied = applied.len(),
        "registry migrations done"
    );
    Ok(applied)
}

/// Apply tenant-scoped pending migrations to every active org.
///
/// Walks `Org::objects().where_(active = true)`, resolves each
/// tenant's storage, and applies every `scope == Tenant` migration
/// from `dir` to that tenant's ledger. Per-tenant atomicity (each
/// migration in its own tx by default); per-tenant failure isolation
/// (one tenant's bad migration doesn't block the rest).
///
/// `registry_url` is the connection string used to spin up the
/// short-lived per-tenant pools for schema-mode tenants. Database-mode
/// tenants use the cached pool from [`TenantPools`] directly; the URL
/// is only needed for schema mode.
///
/// # Errors
/// Walking the Org table or building the scoped subset can short-
/// circuit; returns `Err` in those cases. Per-tenant errors are
/// captured in the [`TenantMigrationReport`] without aborting.
pub async fn migrate_tenants(
    pools: &TenantPools,
    dir: &Path,
    registry_url: &str,
) -> Result<TenantMigrationReport, TenancyError> {
    let scoped = scoped_subset(dir, MigrationScope::Tenant).await?;
    let scoped_path = match &scoped {
        ScopedDir::Owned(temp) => temp.path().to_path_buf(),
        ScopedDir::Original => dir.to_path_buf(),
    };

    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch(pools.registry())
        .await?;

    info!(
        target: "rustango_tenancy",
        tenants = orgs.len(),
        "applying tenant-scoped migrations"
    );

    let mut report = TenantMigrationReport::default();
    for org in &orgs {
        let outcome = run_for_one_tenant(pools, org, &scoped_path, registry_url).await;
        match &outcome {
            Ok(applied) => info!(
                target: "rustango_tenancy",
                slug = %org.slug,
                applied = applied.len(),
                "tenant migrations done"
            ),
            Err(e) => warn!(
                target: "rustango_tenancy",
                slug = %org.slug,
                error = %e,
                "tenant migration failed; continuing with remaining tenants"
            ),
        }
        report.tenants.push(match outcome {
            Ok(applied) => TenantMigrationOutcome {
                slug: org.slug.clone(),
                applied,
                error: None,
            },
            Err(error) => TenantMigrationOutcome {
                slug: org.slug.clone(),
                applied: Vec::new(),
                error: Some(error),
            },
        });
    }
    Ok(report)
}

async fn run_for_one_tenant(
    pools: &TenantPools,
    org: &Org,
    dir: &Path,
    registry_url: &str,
) -> Result<Vec<Migration>, TenancyError> {
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{}` has unknown storage_mode `{got}`",
            org.slug
        ))
    })?;
    match mode {
        StorageMode::Schema => {
            let schema = org
                .schema_name
                .clone()
                .unwrap_or_else(|| org.slug.clone());
            let pool = build_schema_scoped_pool(registry_url, &schema).await?;
            let applied = migrate::migrate(&pool, dir).await?;
            pool.close().await;
            Ok(applied)
        }
        StorageMode::Database => {
            let tenant_pool = pools.pool_for_org(org).await?;
            let applied = migrate::migrate(tenant_pool.pool(), dir).await?;
            Ok(applied)
        }
    }
}

/// Build a short-lived `PgPool` whose every connection has its
/// `search_path` pre-set to `<schema>, public` via an
/// `after_connect` hook. Used only for schema-mode migrations —
/// runtime requests use the shared registry pool with per-checkout
/// `SET`.
///
/// The schema is created if it doesn't exist before the migration
/// runs (so a freshly-provisioned tenant works on its first
/// `migrate_tenants` call). `CREATE SCHEMA IF NOT EXISTS` is
/// idempotent.
async fn build_schema_scoped_pool(
    registry_url: &str,
    schema: &str,
) -> Result<PgPool, TenancyError> {
    // Ensure the schema exists. Use a one-shot connection so we don't
    // pollute the migration pool's connections.
    let bootstrap = PgPool::connect(registry_url).await?;
    let create_sql = format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident_for_schema(schema)
    );
    rustango::sql::sqlx::query(&create_sql)
        .execute(&bootstrap)
        .await?;
    bootstrap.close().await;

    // Now build the migration pool. Every connection gets
    // `SET search_path` once on connect; sqlx caches this per
    // connection so subsequent migration queries against this pool
    // see the right schema without further bookkeeping.
    let schema_owned: Arc<str> = Arc::from(schema);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            let schema = Arc::clone(&schema_owned);
            Box::pin(async move {
                let stmt = format!(
                    "SET search_path TO {}, public",
                    quote_ident_for_schema(&schema)
                );
                rustango::sql::sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect(registry_url)
        .await?;
    Ok(pool)
}

/// Prep a directory of migrations filtered by scope. If every
/// migration in the input dir already matches `scope`, returns
/// [`ScopedDir::Original`] (no copy). Otherwise materializes a
/// temp dir containing only the matching files and returns
/// [`ScopedDir::Owned`].
async fn scoped_subset(dir: &Path, scope: MigrationScope) -> Result<ScopedDir, TenancyError> {
    let all = rustango::migrate::file::list_dir(dir)?;
    if all.iter().all(|m| m.scope == scope) {
        return Ok(ScopedDir::Original);
    }
    let temp = tempdir_under_target()?;
    let temp_path = temp.path().to_path_buf();
    for mig in &all {
        if mig.scope == scope {
            let target = temp_path.join(format!("{}.json", mig.name));
            rustango::migrate::file::write(&target, mig)?;
        }
    }
    Ok(ScopedDir::Owned(temp))
}

enum ScopedDir {
    /// All migrations in `dir` already match the requested scope —
    /// run directly against the original directory.
    Original,
    /// Materialized a temp dir holding only the matching files.
    /// Caller drops to clean up.
    Owned(TempDir),
}

/// Minimal temp-dir RAII handle. We don't pull `tempfile` into the
/// dep tree just for this — `std::env::temp_dir()` + a unique
/// suffix is enough.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir_under_target() -> Result<TempDir, TenancyError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_tenancy_scoped_{pid}_{n}"));
    std::fs::create_dir_all(&p).map_err(|e| {
        TenancyError::Validation(format!(
            "could not create scoped-migration tempdir at {}: {e}",
            p.display()
        ))
    })?;
    Ok(TempDir(p))
}

fn quote_ident_for_schema(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
