//! Multi-tenancy for rustango.
//!
//! v0.5 makes rustango "organizations-aware" without inheriting Django's
//! `DATABASES`-dict-in-`settings.py` footgun. Tenants are first-class
//! rows in a `rustango_orgs` table that lives in the **registry**
//! database — the only database the app boots knowing about. Every
//! other database (or Postgres schema) is discovered through that
//! table at request time.
//!
//! Adding a tenant is `INSERT INTO rustango_orgs (slug, storage_mode,
//! database_url, host_pattern, ...) VALUES (...)`. The next request
//! resolved to that slug builds the pool lazily; no restart, no config
//! change, no redeploy.
//!
//! ## Status
//!
//! v0.5 Slice 1 (this commit) ships only the [`Org`] registry model
//! and a [`TenancyError`] type. Resolvers, [`TenantPools`], scoped
//! migrations, tenant-aware admin, provisioning CLI, and per-tenant
//! auth land in slices 2-7.
//!
//! [`TenantPools`]: pools::TenantPools
//!
//! ## Design choices (locked 2026-04-28)
//!
//! 1. **Operator auth = registry-only.** Two strictly-separated
//!    identity domains. Operators never appear in tenant tables;
//!    org users (even with `is_superuser`) never reach `/operator`.
//! 2. **Slug is globally unique.** Globally — not per-host.
//! 3. **No cross-tenant aggregations.** Out of scope.
//! 4. **Migration scope default = `tenant`.** `registry` is opt-in.
//! 5. **Secrets**: registry DB is the boundary today; pluggable
//!    `SecretsResolver` (slice 3.5) for future vault integrations.
//! 6. **Routing default = subdomain (`acme.app.com`).** Cookie
//!    isolation by subdomain is the headline win. Apex
//!    (`app.com`) routes only to `/operator/*`.
//!
//! See `memory/v05-multitenancy-roadmap.md` in the project memory for
//! the full design and slice plan.

// v0.34 — `tenancy::admin` (the per-tenant admin router builder) is
// PG-only by design because it threads the framework's PgRow-based
// rendering helpers + builds short-lived PgPools with `search_path`
// baked in. Sqlite/MySQL apps that want bundled admin will wait for
// the v0.35+ bi-dialect admin rewrite; today, write your own routes
// using `DatabaseTenant<DB>` + the ORM `_pool` helpers.
#[cfg(feature = "admin")]
pub mod admin;
pub mod auth;
pub mod auth_backends;
pub mod auth_routes;
pub mod bootstrap;
pub mod branding;
pub mod database_pools;
mod error;
pub mod impersonation_handoff;
pub mod jwt_lifecycle;
pub mod manage;
mod manage_interactive;
pub mod middleware;
pub mod migrate;
pub mod operator_console;
mod org;
pub mod password;
pub mod permissions;
mod pools;
mod resolver;
pub mod routes;
mod secrets;
pub mod server;
pub mod tenant_console;

pub use auth::{
    authenticate_operator, authenticate_user, validate_tenant_user_schema, Operator,
    TenantUserModel, User, REQUIRED_USER_COLUMNS,
};
pub use auth_backends::{
    create_api_key, ensure_api_keys_table, ApiKeyBackend, AuthBackend, AuthError, AuthUser,
    BoxedBackend, JwtBackend, ModelBackend,
};
pub use bootstrap::{
    init_tenancy, init_tenancy_with, registry_bootstrap_migration,
    registry_bootstrap_migration_for, tenant_bootstrap_migration, tenant_bootstrap_migration_for,
    InitTenancyReport, REGISTRY_BOOTSTRAP_NAME, TENANT_BOOTSTRAP_NAME,
};
pub use middleware::{AuthenticatedUser, CurrentUser, RouterAuthExt};
pub use permissions::{
    assign_role, auto_create_permissions, clear_user_perm,
    ensure_tables as ensure_permission_tables, get_or_create_role, grant_role_perm, has_all_perms,
    has_any_perm, has_perm, model_codenames, remove_role, revoke_role_perm, set_user_perm,
    user_permissions, user_roles,
};

pub use database_pools::{DatabaseConn, DatabasePool, DatabasePools};
pub use error::TenancyError;
pub use migrate::{
    migrate_registry, migrate_registry_pool, migrate_tenants, TenantMigrationOutcome,
    TenantMigrationReport,
};
pub use org::{BackendKind, Org, StorageMode};
pub use pools::{PrewarmReport, TenantConn, TenantPool, TenantPools, TenantPoolsConfig};
pub use resolver::{
    ChainResolver, HeaderResolver, OrgResolver, PathPrefixResolver, PortResolver, SubdomainResolver,
};
pub use routes::RouteConfig;
pub use secrets::{
    ChainSecretsResolver, EnvSecretsResolver, LiteralSecretsResolver, SecretsError, SecretsResolver,
};
