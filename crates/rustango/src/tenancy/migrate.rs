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
use std::sync::Arc;
use tracing::{info, warn};

#[cfg(feature = "postgres")]
use crate::core::Column as _;
#[cfg(feature = "postgres")]
use crate::migrate;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::postgres::PgPoolOptions;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;
#[cfg(feature = "postgres")]
use crate::sql::Fetcher;

use super::error::TenancyError;
#[cfg(feature = "postgres")]
use super::org::{Org, StorageMode};
#[cfg(feature = "postgres")]
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

/// Apply registry-scoped pending migrations to the registry DB.
///
/// Only migrations whose `scope == Registry` run. Tenant-scoped
/// migrations are silently skipped here — they're for
/// [`migrate_tenants`].
///
/// # Errors
/// As [`crate::migrate::migrate`].
#[cfg(feature = "postgres")]
pub async fn migrate_registry(
    pools: &TenantPools,
    dir: &Path,
) -> Result<Vec<Migration>, TenancyError> {
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
    let applied = match scoped_dir {
        ScopedDir::Owned(temp) => {
            let result = migrate::migrate_pool(registry, temp.path()).await?;
            drop(temp);
            result
        }
        ScopedDir::Original => migrate::migrate_pool(registry, dir).await?,
    };
    // v0.28.4 (#77) — runtime ALTER for the password_changed_at
    // column on Postgres registries. The column landed mid-v0.28; PG
    // registries from earlier versions need it back-filled without a
    // migration JSON the user would have to apply manually. Sqlite +
    // MySQL registries are post-v0.34 and ship the column from the
    // start, so the ALTER is PG-only.
    //
    // v0.37 (#7) — extended to the v0.26+v0.33 `Org` columns the
    // scaffolder's bootstrap JSON predates: `backend_kind` (v0.33
    // multi-backend tenancy), `brand_*` / `logo_path` / `favicon_path`
    // / `primary_color` / `theme_mode` (v0.26 branding). Until the
    // scaffolder templates get regenerated, fresh scaffolded projects
    // need this fixup or `Org` row reads error with `column "..." does
    // not exist`. Each ALTER is idempotent via `ADD COLUMN IF NOT
    // EXISTS`; running on an up-to-date schema is a no-op.
    #[cfg(feature = "postgres")]
    if let Some(pg) = registry.as_postgres() {
        // List every (table, column, type) the current framework
        // expects but a stale bootstrap might be missing. Adding to
        // this list is the contract for "ship a column that older
        // deployments need" — pair every new column with an entry
        // here so users don't have to hand-write ALTERs to upgrade.
        let fixups: &[(&str, &str, &str)] = &[
            (
                "rustango_operators",
                "password_changed_at",
                "TIMESTAMPTZ NULL",
            ),
            (
                "rustango_orgs",
                "backend_kind",
                "VARCHAR(16) NOT NULL DEFAULT 'postgres'",
            ),
            ("rustango_orgs", "brand_name", "VARCHAR(80)"),
            ("rustango_orgs", "brand_tagline", "VARCHAR(200)"),
            ("rustango_orgs", "logo_path", "VARCHAR(120)"),
            ("rustango_orgs", "favicon_path", "VARCHAR(120)"),
            ("rustango_orgs", "primary_color", "VARCHAR(7)"),
            ("rustango_orgs", "theme_mode", "VARCHAR(8)"),
        ];
        for (table, column, col_type) in fixups {
            let sql =
                format!(r#"ALTER TABLE "{table}" ADD COLUMN IF NOT EXISTS "{column}" {col_type}"#);
            if let Err(e) = rustango::sql::sqlx::query(&sql).execute(pg).await {
                // Missing tables (rustango_operators / rustango_orgs)
                // mean the registry bootstrap hasn't run yet — that's
                // a separate error path, not a fixup failure. Warn and
                // continue; the next on-disk migration will create
                // the table and a subsequent boot will re-run this
                // fixup against the populated schema.
                tracing::warn!(
                    target: "crate::tenancy",
                    table = %table,
                    column = %column,
                    error = %e,
                    "registry column fixup failed (non-fatal — re-run after the bootstrap migrate)",
                );
            }
        }
    }
    // Registry-scope audit-log table for operator-side actions
    // (impersonation start / end, org config edits via the
    // operator console, etc.). Bi-dialect via
    // `audit::ensure_table_pool` (Postgres / MySQL / SQLite all
    // supported).
    if let Err(e) = crate::audit::ensure_table_pool(registry).await {
        tracing::warn!(
            target: "crate::tenancy",
            error = %e,
            "audit::ensure_table_pool failed for registry pool",
        );
    }
    // (#89) Auto-seed the `rustango_content_types` registry-side
    // catalog — the operator console's audit log + permissions UI
    // consult it to resolve `entity_table` strings back to a stable
    // per-model identifier. Bi-dialect via
    // `contenttypes::ensure_seeded_pool` (v0.34 slice 1).
    if let Err(e) = crate::contenttypes::ensure_seeded_pool(registry).await {
        tracing::warn!(
            target: "crate::tenancy",
            error = %e,
            "contenttypes::ensure_seeded_pool failed for registry pool",
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
        .fetch(pools.registry())
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
            let applied = migrate::migrate(&pool, dir).await?;
            // v0.13.0: ensure the per-tenant audit log table exists so
            // projects don't have to call `audit::ensure_table` from
            // their seed manually. Best-effort — failures here log a
            // warning but don't fail the migration.
            if let Err(e) = crate::audit::ensure_table(&pool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "audit::ensure_table failed for schema-mode tenant");
            }
            if let Err(e) = super::permissions::ensure_tables(&pool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "permissions::ensure_tables failed for schema-mode tenant");
            }
            // v0.27.2 — seed the `rustango_permissions` catalog with
            // the four CRUD codenames for every registered Model
            // whose `permissions` flag is on. Idempotent. Without
            // this, non-superuser tenant admins could never view
            // scaffolded models because `is_visible(table)` checks
            // `{table}.view ∈ user_perms` and the catalog row is
            // the prerequisite for any role to grant it. (#61)
            if let Err(e) = super::permissions::auto_create_permissions(&pool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "auto_create_permissions failed for schema-mode tenant");
            }
            // (#89) Auto-seed `rustango_content_types` from the
            // inventory registry — the operator-facing CT catalog
            // every framework feature reaching for "any model"
            // (audit log, generic FKs, permissions UI) consults.
            // Idempotent; pre-existing rows are unchanged thanks
            // to the UNIQUE(app_label, model_name) constraint.
            if let Err(e) = crate::contenttypes::ensure_seeded(&pool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for schema-mode tenant");
            }
            if let Err(e) = super::auth_backends::ensure_api_keys_table(&pool).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "ensure_api_keys_table failed for schema-mode tenant");
            }
            pool.close().await;
            Ok(applied)
        }
        StorageMode::Database => {
            let tenant_pool = pools.pool_for_org(org).await?;
            let applied = migrate::migrate(tenant_pool.pool(), dir).await?;
            if let Err(e) = crate::audit::ensure_table(tenant_pool.pool()).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "audit::ensure_table failed for database-mode tenant");
            }
            if let Err(e) = super::permissions::ensure_tables(tenant_pool.pool()).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "permissions::ensure_tables failed for database-mode tenant");
            }
            // See schema-mode comment above (#61).
            if let Err(e) = super::permissions::auto_create_permissions(tenant_pool.pool()).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "auto_create_permissions failed for database-mode tenant");
            }
            // (#89) See schema-mode comment above.
            if let Err(e) = crate::contenttypes::ensure_seeded(tenant_pool.pool()).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for database-mode tenant");
            }
            if let Err(e) = super::auth_backends::ensure_api_keys_table(tenant_pool.pool()).await {
                tracing::warn!(target: "crate::tenancy", slug = %org.slug, error = %e, "ensure_api_keys_table failed for database-mode tenant");
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

fn quote_ident_for_schema(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
