//! Typed Rust API for tenancy provisioning.
//!
//! Mirrors the manage-CLI verbs (`create-tenant`, `create-operator`,
//! `create-user`) but with structured argument types and direct return
//! values — for use from `Builder::seed_with` closures and any other
//! in-process caller. CLI-style verb dispatch via
//! [`super::run_with_writer`] is unchanged for shell consumers.
//!
//! Every function suffixed `_if_missing` is **idempotent** — if a row
//! with the requested key already exists, the existing row is
//! returned without error. This is the canonical seed/bootstrap
//! shape: re-running the demo or a deployment hook does not error.

use std::path::Path;

use sqlx::Database;

use crate::core::Column as _;
use crate::sql::{Auto, FetcherPool};
use crate::tenancy::auth::{Operator, User};
use crate::tenancy::error::TenancyError;
use crate::tenancy::migrate as tenant_migrate;
use crate::tenancy::org::{Org, StorageMode};
use crate::tenancy::pools::TenantPools;

/// Inputs for [`create_tenant`] / [`create_tenant_if_missing`].
///
/// Defaults: `mode = Schema`, all `Option`s `None`, `no_migrate =
/// false`. When `host_pattern` is `None`, the default is
/// `"{slug}.{RUSTANGO_APEX_DOMAIN}"` (read from env at provisioning
/// time) — same convention the CLI verb uses.
pub struct CreateTenantOpts {
    pub mode: StorageMode,
    /// Backend driver (v0.33+). Defaults to `Postgres` for
    /// back-compat. `validate_storage_mode` is called against the
    /// `(mode, backend)` pair before the row is inserted — see
    /// [`crate::tenancy::BackendKind::validate_storage_mode`].
    pub backend: crate::tenancy::BackendKind,
    pub display_name: Option<String>,
    pub schema_name: Option<String>,
    pub database_url: Option<String>,
    pub host_pattern: Option<String>,
    pub port: Option<i32>,
    pub path_prefix: Option<String>,
    pub no_migrate: bool,
}

impl Default for CreateTenantOpts {
    fn default() -> Self {
        Self {
            mode: StorageMode::Schema,
            backend: crate::tenancy::BackendKind::Postgres,
            display_name: None,
            schema_name: None,
            database_url: None,
            host_pattern: None,
            port: None,
            path_prefix: None,
            no_migrate: false,
        }
    }
}

/// Look up an org by slug. Returns `Ok(None)` for "no such tenant" —
/// not an error.
///
/// # Errors
/// Driver / SQL failures during the lookup.
pub async fn find_org<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
) -> Result<Option<Org>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(&pools.registry_pool())
        .await?;
    Ok(rows.pop())
}

/// Provision a new tenant and run tenant-scoped migrations against it.
/// Errors if a tenant with this slug already exists.
///
/// # Errors
/// * `TenancyError::Validation` for duplicate slug or bad
///   schema/database options.
/// * Driver / SQL failures during schema creation, the Org INSERT, or
///   the tenant migration pass.
pub async fn create_tenant<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    migrations_dir: &Path,
    slug: &str,
    opts: CreateTenantOpts,
) -> Result<Org, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    if find_org(pools, slug).await?.is_some() {
        return Err(TenancyError::Validation(format!(
            "tenant slug `{slug}` already exists"
        )));
    }

    if opts.mode == StorageMode::Database && opts.database_url.is_none() {
        return Err(TenancyError::Validation(
            "create_tenant: mode=Database requires database_url".into(),
        ));
    }
    // v0.33 — reject schema-mode for non-Postgres backends. MySQL +
    // SQLite must use database-mode.
    opts.backend
        .validate_storage_mode(opts.mode)
        .map_err(|msg| TenancyError::Validation(msg.to_owned()))?;

    let host_pattern = opts.host_pattern.clone().or_else(|| {
        std::env::var("RUSTANGO_APEX_DOMAIN")
            .ok()
            .map(|apex| format!("{slug}.{apex}"))
    });
    let display_name = opts.display_name.clone().unwrap_or_else(|| slug.to_owned());
    let schema_name = match opts.mode {
        StorageMode::Schema => Some(opts.schema_name.clone().unwrap_or_else(|| slug.to_owned())),
        StorageMode::Database => None,
    };

    if let StorageMode::Schema = opts.mode {
        #[cfg(feature = "postgres")]
        {
            let pg_pools = (pools as &dyn std::any::Any)
                .downcast_ref::<TenantPools<sqlx::Postgres>>()
                .ok_or_else(|| {
                    TenancyError::Validation(
                        "schema-mode tenants require a Postgres registry".into(),
                    )
                })?;
            let schema = schema_name.as_deref().unwrap_or(slug);
            let sql = format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                super::args::quote_ident(schema)
            );
            rustango::sql::sqlx::query(&sql)
                .execute(pg_pools.registry())
                .await?;
        }
        #[cfg(not(feature = "postgres"))]
        {
            return Err(TenancyError::Validation(
                "schema-mode tenants require the `postgres` feature".into(),
            ));
        }
    }

    let mut org = Org {
        id: Auto::default(),
        slug: slug.to_owned(),
        display_name,
        storage_mode: opts.mode.as_str().into(),
        backend_kind: opts.backend.as_str().into(),
        database_url: opts.database_url.clone(),
        schema_name,
        host_pattern,
        port: opts.port,
        path_prefix: opts.path_prefix.clone(),
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&pools.registry_pool()).await?;

    if !opts.no_migrate {
        for dir in resolve_migration_dirs(migrations_dir) {
            // PG path runs schema-mode + database-mode; non-PG runs
            // database-mode only via migrate_tenants_db.
            #[cfg(feature = "postgres")]
            {
                if let Some(pg_pools) =
                    (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
                {
                    let _ = tenant_migrate::migrate_tenants(pg_pools, &dir, registry_url).await?;
                } else {
                    let _ = tenant_migrate::migrate_tenants_db(pools, &dir, registry_url).await?;
                }
            }
            #[cfg(not(feature = "postgres"))]
            {
                let _ = tenant_migrate::migrate_tenants_db(pools, &dir, registry_url).await?;
            }
        }
    }

    // Re-fetch so the returned Org has whatever the runner / triggers
    // wrote back; cheap, and lets callers chain `pools.acquire(&org)`
    // immediately.
    find_org(pools, slug).await?.ok_or_else(|| {
        TenancyError::Validation(format!(
            "create_tenant: tenant `{slug}` vanished after INSERT"
        ))
    })
}

/// Idempotent variant of [`create_tenant`]: returns the existing org
/// if one already has this slug, otherwise creates + returns the new
/// one. The seed/bootstrap default.
///
/// # Errors
/// Driver / SQL failures, or [`create_tenant`]'s validation errors
/// (mode/url mismatch).
pub async fn create_tenant_if_missing<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    migrations_dir: &Path,
    slug: &str,
    opts: CreateTenantOpts,
) -> Result<Org, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    if let Some(existing) = find_org(pools, slug).await? {
        return Ok(existing);
    }
    create_tenant(pools, registry_url, migrations_dir, slug, opts).await
}

/// Idempotent variant of `create-operator`: creates the operator if
/// no row with this username exists, else returns the existing one.
///
/// # Errors
/// Driver / SQL failures or password-hash failures.
pub async fn create_operator_if_missing<DB: Database>(
    pools: &TenantPools<DB>,
    username: &str,
    password: &str,
) -> Result<Operator, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let mut existing: Vec<Operator> = Operator::objects()
        .where_(Operator::username.eq(username.to_owned()))
        .fetch(&registry)
        .await?;
    if let Some(op) = existing.pop() {
        return Ok(op);
    }
    let mut op = Operator {
        id: Auto::default(),
        username: username.to_owned(),
        password_hash: crate::tenancy::password::hash(password)?,
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
    };
    op.insert_pool(&registry).await?;
    Ok(op)
}

/// Idempotent variant of `create-user`: creates a tenant-scoped user
/// if no row with this username exists in the tenant's schema, else
/// returns the existing one.
///
/// # Errors
/// * `TenancyError::Validation` if the tenant slug is unknown.
/// * Driver / SQL failures.
pub async fn create_user_if_missing<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    username: &str,
    password: &str,
    superuser: bool,
) -> Result<User, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let org = find_org(pools, slug)
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("no tenant with slug `{slug}`")))?;

    // v0.38 — route through the backend-agnostic
    // `TenantPools::scoped_pool_dyn` (schema-mode-aware on PG, db-mode
    // elsewhere) + ORM `_pool` family so the same code runs on every
    // dialect.
    let scoped = pools.scoped_pool_dyn(&org).await?;

    let mut existing: Vec<User> = User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch(&scoped)
        .await?;
    if let Some(u) = existing.pop() {
        return Ok(u);
    }
    let mut user = User {
        id: Auto::default(),
        username: username.to_owned(),
        password_hash: crate::tenancy::password::hash(password)?,
        is_superuser: superuser,
        active: true,
        created_at: chrono::Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
    };
    user.save_pool(&scoped).await?;
    Ok(user)
}

/// Resolve the path the caller passed to [`create_tenant`] /
/// [`create_tenant_if_missing`] into the actual list of migrations
/// directories to run against. Mirrors the auto-detect in
/// [`crate::server::Builder::migrate`] so the typed API and the
/// builder accept the same shape.
///
/// Cases:
/// * `dir` itself contains `*.json` migrations — treat it as a flat
///   migrations directory and run against it directly.
/// * `dir` contains a `migrations/` subdir or per-app `<app>/migrations/`
///   subdirs — treat `dir` as the project root and discover.
/// * Anything else — return `[dir]` and let the runner produce its
///   normal "no migrations to apply" outcome rather than silently
///   pretending the apply happened.
fn resolve_migration_dirs(dir: &Path) -> Vec<std::path::PathBuf> {
    let dirs = crate::migrate::file::discover_migration_dirs(dir);
    if !dirs.is_empty() {
        return dirs;
    }
    if dir_has_json_files(dir) {
        return vec![dir.to_path_buf()];
    }
    vec![dir.to_path_buf()]
}

fn dir_has_json_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
}
