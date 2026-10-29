//! Typed Rust API for tenancy provisioning.
//!
//! Mirrors the manage-CLI verbs (`create-tenant`, `create-operator`,
//! `create-user`) but with structured argument types and direct return
//! values — for use from `Builder::seed_with` closures and any other
//! in-process caller. CLI-style verb dispatch via
//! [`run_with_writer`](crate::tenancy::manage::run_with_writer) is
//! unchanged for shell consumers.
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
use crate::tenancy::org::{Org, StorageMode};
use crate::tenancy::pools::TenantPools;
use crate::tenancy::provision::{self, DatabaseSource, TenantStorage};

/// Inputs for [`create_tenant`] / [`create_tenant_if_missing`].
///
/// Defaults: `mode = Schema`, all `Option`s `None`, `no_migrate =
/// false`. When `host_pattern` is `None` and no `path_prefix` is set, the
/// default is `"{slug}.{RUSTANGO_APEX_DOMAIN}"` (read from env at
/// provisioning time); with a `path_prefix` the tenant gets no host and is
/// served at `<apex>/<prefix>/` — same convention the CLI verb uses.
pub struct CreateTenantOpts {
    pub mode: StorageMode,
    /// `None` takes the registry's; anything else must equal it.
    pub backend: Option<crate::tenancy::BackendKind>,
    pub display_name: Option<String>,
    pub schema_name: Option<String>,
    /// An existing database. Database mode needs this or `database_server`.
    pub database_url: Option<String>,
    /// Create the database, named as the slug, on this saved server.
    pub database_server: Option<String>,
    pub host_pattern: Option<String>,
    pub port: Option<i32>,
    pub path_prefix: Option<String>,
    pub no_migrate: bool,
    /// A superuser created before the tenant goes live. Issue their
    /// set-password link with [`crate::tenancy::first_admin::hand_over_user`].
    pub first_admin: Option<crate::tenancy::FirstAdmin>,
}

impl Default for CreateTenantOpts {
    fn default() -> Self {
        Self {
            mode: StorageMode::Schema,
            backend: None,
            display_name: None,
            schema_name: None,
            database_url: None,
            database_server: None,
            host_pattern: None,
            port: None,
            path_prefix: None,
            no_migrate: false,
            first_admin: None,
        }
    }
}

impl CreateTenantOpts {
    /// The one storage these options name.
    fn storage(&self) -> Result<TenantStorage, TenancyError> {
        let bad = |m: &str| Err(TenancyError::Validation(format!("create_tenant: {m}")));
        match (self.mode, &self.database_url, &self.database_server) {
            (StorageMode::Schema, None, None) => {
                Ok(TenantStorage::Schema(self.schema_name.clone()))
            }
            (StorageMode::Schema, _, _) => bad("schema mode takes no database URL or server"),
            (StorageMode::Database, Some(url), None) => {
                Ok(TenantStorage::Database(DatabaseSource::Url(url.clone())))
            }
            (StorageMode::Database, None, Some(server)) => Ok(TenantStorage::Database(
                DatabaseSource::Server(server.clone()),
            )),
            (StorageMode::Database, Some(_), Some(_)) => {
                bad("give database_url or database_server, not both")
            }
            (StorageMode::Database, None, None) => {
                bad("mode=Database requires database_url or database_server")
            }
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
/// Errors if a tenant with this slug already exists. On a saved server,
/// a failure after the create drops the new database, login and row.
///
/// # Errors
/// * `TenancyError::Validation` for duplicate slug or bad
///   schema/database options.
/// * Driver / SQL failures during schema creation, the Org INSERT, or
///   the tenant migration pass. A failed migration leaves the Org inactive.
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
    // Detached: a dropped caller must not stop a create between its DDL
    // and the rows that record it.
    let pools = pools.fresh();
    let (url, dir, slug) = (
        registry_url.to_owned(),
        migrations_dir.to_owned(),
        slug.to_owned(),
    );
    tokio::spawn(async move { create_tenant_inner(&pools, &url, &dir, &slug, opts).await })
        .await
        .map_err(|e| TenancyError::Validation(format!("the create task stopped: {e}")))?
}

async fn create_tenant_inner<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    migrations_dir: &Path,
    slug: &str,
    opts: CreateTenantOpts,
) -> Result<Org, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let request = provision::ProvisionRequest {
        backend: opts.backend,
        display_name: opts.display_name.clone(),
        host_pattern: opts.host_pattern.clone(),
        port: opts.port,
        path_prefix: opts.path_prefix.clone(),
        run_migrations: !opts.no_migrate,
        first_admin: opts.first_admin.clone(),
        ..provision::ProvisionRequest::new(slug, opts.storage()?)
    };
    let (_run, outcome) = provision::provision_tenant_recorded(
        pools,
        registry_url,
        migrations_dir,
        &request,
        None,
        None,
        None,
    )
    .await?;
    created_org(pools, slug, outcome).await
}

/// The engine's outcome as this API reports it: a failed migration is an
/// `Err`, and the row is re-read so callers can `pools.acquire` it at once.
async fn created_org<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    outcome: provision::ProvisionOutcome,
) -> Result<Org, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    if let provision::MigrationsOutcome::Failed(e) = outcome.migrations {
        return Err(TenancyError::Validation(format!(
            "tenant `{slug}` migrations failed; it stays inactive: {e}"
        )));
    }
    find_org(pools, slug).await?.ok_or_else(|| {
        TenancyError::Validation(format!(
            "create_tenant: tenant `{slug}` vanished after INSERT"
        ))
    })
}

/// Idempotent variant of [`create_tenant`]: returns the existing org
/// if one already has this slug, otherwise creates + returns the new
/// one. The seed/bootstrap default. An org whose create failed at
/// migrate is migrated and activated instead.
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
    let Some(existing) = find_org(pools, slug).await? else {
        return create_tenant(pools, registry_url, migrations_dir, slug, opts).await;
    };
    // Finish a create whose migrations failed; a suspended tenant stays as is.
    let registry = pools.registry_pool();
    let org_id = existing.id.get().copied().unwrap_or_default();
    if existing.active
        || !crate::tenancy::provision_store::org_left_by_failed_run(&registry, org_id).await?
    {
        return Ok(existing);
    }
    // The engine's resume, with the storage the failed create stored.
    let mode = StorageMode::parse(&existing.storage_mode)
        .map_err(|m| TenancyError::Validation(format!("unknown storage mode `{m}`")))?;
    let storage = match mode {
        StorageMode::Schema => TenantStorage::Schema(existing.schema_name.clone()),
        StorageMode::Database => TenantStorage::Database(DatabaseSource::Url(
            existing.database_url.clone().unwrap_or_default(),
        )),
    };
    let request = provision::ProvisionRequest {
        run_migrations: !opts.no_migrate,
        first_admin: opts.first_admin.clone(),
        ..provision::ProvisionRequest::new(slug, storage)
    };
    let run = crate::tenancy::provision_store::open_run(
        &registry,
        slug,
        &existing.storage_mode,
        &existing.backend_kind,
        None,
        None,
        None,
    )
    .await?;
    let run_id = run.id.get().copied().unwrap_or_default();
    let outcome = provision::provision_tenant_in_run(
        pools,
        registry_url,
        migrations_dir,
        &request,
        None,
        run_id,
    )
    .await?;
    let org = created_org(pools, slug, outcome).await?;
    let forget = crate::tenancy::provision_store::forget_unsucceeded_runs(org_id)?;
    crate::sql::update_pool(&registry, &forget).await?;
    Ok(org)
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
        password_hash: crate::tenancy::password::hash_async(password).await?,
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
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
    crate::tenancy::auth::NewUser {
        username,
        password_hash: crate::tenancy::password::hash_async(password).await?,
        is_superuser: superuser,
        email: None,
    }
    .insert(&scoped)
    .await
}
