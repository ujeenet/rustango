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

use crate::migrate::{Migration, MigrationScope};
use std::path::Path;
#[cfg(feature = "postgres")]
use std::sync::Arc;
use tracing::{info, warn};

use crate::core::Column as _;
use crate::migrate;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::postgres::PgPoolOptions;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;
use sqlx::Database;

use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::pools::TenantPools;

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

/// Ledger table tracking the framework's own ("system app") migrations,
/// kept separate from the project's `__rustango_migrations__` so the two
/// chains never collide.
const SYSTEM_LEDGER: &str = "__rustango_system_migrations__";

/// Generate (from the current models, if not already on disk) and apply
/// the framework's system-app migrations for `scope` against `pool`.
///
/// This replaces the old hand-written `ensure_*` / bootstrap / ALTER-fixup
/// DDL: the framework's own tables come from makemigrations-generated
/// files (drift-free and feature-`#[cfg]`-aware) in
/// `<project_root>/system/migrations/`, applied under [`SYSTEM_LEDGER`].
/// Generation is a no-op once the files exist (committed, or generated on
/// a previous run); a read-only tree simply means they were committed.
async fn apply_system_migrations(
    pool: &crate::sql::Pool,
    dir: &Path,
    scope: crate::core::ModelScope,
) -> Result<Vec<Migration>, TenancyError> {
    // `system/migrations/` is a sibling of the project's `migrations/`
    // dir. When `dir` is literally `<root>/migrations`, the project root
    // is its parent; otherwise (e.g. a bare test dir) keep `system/`
    // contained inside `dir` rather than polluting its parent.
    let project_root = if dir.file_name().and_then(|n| n.to_str()) == Some("migrations") {
        dir.parent().unwrap_or(dir)
    } else {
        dir
    };
    // Best-effort generate from the compiled models (Ok(None) when the
    // on-disk system migrations already match the models).
    let _ = crate::migrate::make_migrations_system(project_root, scope, None);
    let system_dir = project_root.join("system").join("migrations");
    if !system_dir.is_dir() {
        return Ok(Vec::new());
    }
    let migration_scope = match scope {
        crate::core::ModelScope::Registry => MigrationScope::Registry,
        crate::core::ModelScope::Tenant => MigrationScope::Tenant,
    };
    let applied = match scoped_subset(&system_dir, migration_scope).await? {
        ScopedDir::Owned(temp) => {
            let r =
                crate::migrate::migrate_pool_with_ledger(pool, temp.path(), SYSTEM_LEDGER).await?;
            drop(temp);
            r
        }
        ScopedDir::Original => {
            crate::migrate::migrate_pool_with_ledger(pool, &system_dir, SYSTEM_LEDGER).await?
        }
    };
    Ok(applied)
}

/// Apply registry-scoped pending migrations to the registry DB.
///
/// Only migrations whose `scope == Registry` run. Tenant-scoped
/// migrations are silently skipped here — they're for
/// [`migrate_tenants`].
///
/// # Errors
/// As [`crate::migrate::migrate`].
/// v0.38 — generic over the registry backend. Routes through the
/// tri-dialect [`migrate_registry_pool`].
pub async fn migrate_registry<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
) -> Result<Vec<Migration>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    migrate_registry_pool(&pools.registry_pool(), dir).await
}

/// Backend-agnostic registry migration runner — counterpart of
/// [`migrate_registry`] that takes a [`crate::sql::Pool`] enum
/// directly instead of going through [`TenantPools`]. Routes the
/// migration runner, audit-table bootstrap, and contenttype seed
/// through their backend-agnostic `_pool` variants so a sqlite /
/// mysql registry works end-to-end.
///
/// The PG-only password-changed-at ALTER stays gated to Postgres —
/// it only matters for registries upgraded from pre-v0.28.4, and
/// fresh non-PG registries are never in that state.
///
/// # Errors
/// As [`crate::migrate::migrate_pool`].
pub async fn migrate_registry_pool(
    registry: &crate::sql::Pool,
    dir: &Path,
) -> Result<Vec<Migration>, TenancyError> {
    info!(target: "crate::tenancy", "applying registry-scoped migrations");
    let scoped_dir = scoped_subset(dir, MigrationScope::Registry).await?;
    let mut applied = match scoped_dir {
        ScopedDir::Owned(temp) => {
            let result = crate::migrate::migrate_pool(registry, temp.path()).await?;
            drop(temp);
            result
        }
        ScopedDir::Original => crate::migrate::migrate_pool(registry, dir).await?,
    };
    // The framework's own registry tables (rustango_orgs, rustango_operators,
    // rustango_admin_users) come from makemigrations-generated system-app
    // migrations — no hand-written bootstrap/ensure/ALTER DDL.
    applied
        .extend(apply_system_migrations(registry, dir, crate::core::ModelScope::Registry).await?);
    // (#89) Auto-seed the `rustango_content_types` registry-side
    // catalog — the operator console's audit log + permissions UI
    // consult it to resolve `entity_table` strings back to a stable
    // per-model identifier. Bi-dialect via
    // `contenttypes::ensure_seeded` (v0.34 slice 1).
    if let Err(e) = crate::contenttypes::ensure_seeded(registry).await {
        tracing::warn!(
            target: "crate::tenancy",
            error = %e,
            "contenttypes::ensure_seeded failed for registry pool",
        );
    }
    info!(
        target: "crate::tenancy",
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
#[cfg(feature = "postgres")]
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
    let migrations_in_scope = rustango::migrate::file::list_dir(&scoped_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch_on(pools.registry())
        .await?;

    info!(
        target: "crate::tenancy",
        tenants = orgs.len(),
        migrations = migrations_in_scope,
        dir = %dir.display(),
        "applying tenant-scoped migrations"
    );
    if migrations_in_scope == 0 && !orgs.is_empty() {
        // Surface the most likely cause for an `applied=0` report: caller
        // passed a path with no tenant-scoped migrations. Common footgun
        // is passing the project root when a flat `migrations/` subdir
        // was meant. The typed `tenancy::manage::api` auto-detects via
        // `resolve_migration_dirs`, but raw callers can still hit this.
        warn!(
            target: "crate::tenancy",
            dir = %dir.display(),
            "no tenant-scoped migrations found in dir; tenants will record applied=0 — \
             pass the flat migrations directory or a project root containing one"
        );
    }

    let mut report = TenantMigrationReport::default();
    for org in &orgs {
        let outcome = run_for_one_tenant(pools, org, &scoped_path, registry_url).await;
        match &outcome {
            Ok(applied) => info!(
                target: "crate::tenancy",
                slug = %org.slug,
                applied = applied.len(),
                "tenant migrations done"
            ),
            Err(e) => warn!(
                target: "crate::tenancy",
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

/// v0.38 — tri-dialect counterpart of [`migrate_tenants`]. Walks
/// active orgs from the registry (any backend) and applies
/// tenant-scoped migrations. Schema-mode tenants are rejected
/// (schema-mode is PG-only by language); database-mode tenants
/// migrate against their per-tenant pool via
/// [`TenantPools::database_acquire`].
///
/// `_registry_url` is accepted for API symmetry with
/// [`migrate_tenants`] but unused on this path — database-mode
/// migrations don't need to spin up a schema-scoped pool.
///
/// # Errors
/// Walking the Org table can short-circuit; per-tenant errors are
/// captured in the [`TenantMigrationReport`] without aborting.
pub async fn migrate_tenants_db<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    _registry_url: &str,
) -> Result<TenantMigrationReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::sql::FetcherPool as _;
    let scoped = scoped_subset(dir, MigrationScope::Tenant).await?;
    let scoped_path = match &scoped {
        ScopedDir::Owned(temp) => temp.path().to_path_buf(),
        ScopedDir::Original => dir.to_path_buf(),
    };

    let registry_pool = pools.registry_pool();
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&registry_pool)
        .await?;

    info!(
        target: "crate::tenancy",
        tenants = orgs.len(),
        dir = %dir.display(),
        "applying tenant-scoped migrations (db-mode only)"
    );

    let mut report = TenantMigrationReport::default();
    for org in &orgs {
        let outcome = run_for_one_tenant_db(pools, org, &scoped_path).await;
        match &outcome {
            Ok(applied) => info!(
                target: "crate::tenancy",
                slug = %org.slug,
                applied = applied.len(),
                "tenant migrations done"
            ),
            Err(e) => warn!(
                target: "crate::tenancy",
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

async fn run_for_one_tenant_db<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    dir: &Path,
) -> Result<Vec<Migration>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{}` has unknown storage_mode `{got}`",
            org.slug
        ))
    })?;
    if !matches!(mode, StorageMode::Database) {
        return Err(TenancyError::Validation(format!(
            "org `{}` is schema-mode but migrate_tenants_db only handles \
             database-mode tenants (schema-mode is PG-only by language)",
            org.slug,
        )));
    }
    // Get a tenant-scoped Pool enum (no SET search_path needed for
    // database-mode), apply migrations through `migrate_pool`, then
    // run the audit/permission/contenttype/api-key DDL backfills
    // through the same backend-agnostic helpers the registry-side
    // bootstrap uses.
    let tenant_pool = pools.database_pool_for_org(org).await?;
    let inner_pool = match &tenant_pool {
        super::pools::TenantPool::Database { pool } => crate::sql::Pool::from((**pool).clone()),
        #[cfg(feature = "postgres")]
        super::pools::TenantPool::Schema { .. } => {
            unreachable!("database_pool_for_org rejects schema-mode")
        }
    };
    // Framework tenant tables (users, roles, permissions, api_keys,
    // audit_log, content_types) come from the makemigrations-generated
    // system-app migrations and MUST be applied BEFORE the tenant's user
    // migrations, which may FK into them (issue #1171).
    let mut applied =
        apply_system_migrations(&inner_pool, dir, crate::core::ModelScope::Tenant).await?;
    ensure_media_tables(&inner_pool).await?;
    applied.extend(migrate::migrate_pool(&inner_pool, dir).await?);
    // Data seeders (rows, not DDL — kept): the CRUD permission codenames
    // for every registered model (#61) + the content-type catalog (#89).
    if let Err(e) = super::permissions::auto_create_permissions_pool(&inner_pool).await {
        tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "auto_create_permissions_pool failed for database-mode tenant");
    }
    if let Err(e) = crate::contenttypes::ensure_seeded(&inner_pool).await {
        tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for database-mode tenant");
    }
    Ok(applied)
}

/// Tri-dialect tenant migration dispatch (v0.38). On PG, downcasts
/// to `TenantPools<sqlx::Postgres>` and calls [`migrate_tenants`]
/// (which handles both schema-mode + database-mode tenants); on any
/// other backend, calls [`migrate_tenants_db`] (database-mode only —
/// schema-mode is PG-only by language).
///
/// Used by [`crate::server::Builder::migrate`] so generic backends
/// share the same Builder entry-point.
///
/// # Errors
/// As [`migrate_tenants`] / [`migrate_tenants_db`].
pub async fn migrate_tenants_dyn<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
) -> Result<TenantMigrationReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    #[cfg(feature = "postgres")]
    if let Some(pg) = (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>() {
        return migrate_tenants(pg, dir, registry_url).await;
    }
    migrate_tenants_db(pools, dir, registry_url).await
}

/// Ensure the framework `media` subsystem tables (`rustango_media`,
/// `rustango_media_collections`, `rustango_media_tags`) exist in a
/// tenant's storage. Media is not yet part of the migration engine
/// (#1174) — it ships idempotent `CREATE TABLE IF NOT EXISTS` DDL via
/// [`crate::media::ensure_all_tables_pool`]. We run it here, AFTER the
/// system migrations and BEFORE the tenant's user migrations, so a tenant
/// model that FKs `rustango_media` applies cleanly on a fresh tenant.
/// No-op unless the `media` feature is enabled.
async fn ensure_media_tables(pool: &crate::sql::Pool) -> Result<(), TenancyError> {
    #[cfg(feature = "media")]
    crate::media::ensure_all_tables_pool(pool)
        .await
        .map_err(|e| TenancyError::Validation(format!("media table ensure failed: {e}")))?;
    #[cfg(not(feature = "media"))]
    let _ = pool;
    Ok(())
}

#[cfg(feature = "postgres")]
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
            let schema = org.schema_name.clone().unwrap_or_else(|| org.slug.clone());
            let pool = build_schema_scoped_pool(registry_url, &schema).await?;
            // Framework tenant tables (rustango_users/roles/permissions/…)
            // come from the system-app migrations and MUST be applied
            // BEFORE the tenant's user migrations, which may FK into them
            // (issue #1171). Applying user migrations first breaks a fresh
            // tenant whose model references e.g. rustango_users.
            let dbpool: crate::sql::Pool = pool.clone().into();
            let mut applied =
                apply_system_migrations(&dbpool, dir, crate::core::ModelScope::Tenant).await?;
            ensure_media_tables(&dbpool).await?;
            applied.extend(migrate::migrate(&pool, dir).await?);
            // Data seeders (rows, not DDL — kept): CRUD permission
            // codenames for every registered model (#61) + the
            // content-type catalog (#89). Idempotent.
            if let Err(e) = super::permissions::auto_create_permissions(&pool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "auto_create_permissions failed for schema-mode tenant");
            }
            if let Err(e) = crate::contenttypes::ensure_seeded(&dbpool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for schema-mode tenant");
            }
            pool.close().await;
            Ok(applied)
        }
        StorageMode::Database => {
            let tenant_pool = pools.pool_for_org(org).await?;
            // System-app migrations before user migrations (issue #1171).
            let dbpool: crate::sql::Pool = tenant_pool.pool().clone().into();
            let mut applied =
                apply_system_migrations(&dbpool, dir, crate::core::ModelScope::Tenant).await?;
            ensure_media_tables(&dbpool).await?;
            applied.extend(migrate::migrate(tenant_pool.pool(), dir).await?);
            // Data seeders (rows, not DDL — kept): #61 + #89.
            if let Err(e) = super::permissions::auto_create_permissions(tenant_pool.pool()).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "auto_create_permissions failed for database-mode tenant");
            }
            if let Err(e) = crate::contenttypes::ensure_seeded(&dbpool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for database-mode tenant");
            }
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
#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
fn quote_ident_for_schema(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
