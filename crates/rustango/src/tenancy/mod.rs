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
#[cfg(all(feature = "admin", feature = "postgres"))]
pub mod admin;
pub mod auth;
pub mod auth_backends;
// v0.38 — `auth_routes` uses the PG-only `Tenant` extractor for
// `per-tenant JWT routes; sqlite/mysql tenancy projects build their
// own routes using `DatabaseTenant<DB>` until slice 4 lifts `Tenant`
// to be generic.
#[cfg(feature = "postgres")]
pub mod auth_routes;
pub mod bootstrap;
pub mod branding;
pub mod database_pools;
mod error;
// v0.38 — depends on operator_console signed cookies + TenantPools.
#[cfg(feature = "postgres")]
pub mod impersonation_handoff;
pub mod jwt_lifecycle;
// v0.38 — manage CLI (create-tenant / create-operator / create-user
// / role+perm verbs) is wired through the audit cleanup helpers,
// tenant migration runner, and operator console — all PG-typed
// internals. The runtime tri-dialect path goes through
// DatabasePools<DB> directly; the CLI submodules wait for their
// _pool lifts (slice 9+).
#[cfg(feature = "postgres")]
pub mod manage;
mod manage_interactive;
pub mod middleware;
pub mod migrate;
// v0.38 — `session` (HMAC-signed session cookies, SessionSecret type)
// has no PG deps and ships unconditionally; `DatabaseTenantContext`
// references it. The wider `operator_console` module wires
// TenantPools + admin builder and stays PG-gated until slice 5.
#[cfg(feature = "postgres")]
pub mod operator_console;
mod org;
pub mod password;
pub mod permissions;
mod pools;
mod resolver;
pub mod routes;
mod secrets;
pub mod session;
// v0.38 — `tenancy::server` ties together TenantPools + Tenant
// extractor + admin builder; all PG-typed today. Gated to PG; sqlite/
// mysql apps assemble their own routes today, full generic lift in
// slice 5 (server::Builder<DB>).
#[cfg(feature = "postgres")]
pub mod server;
// v0.38 — tenant console mounts the per-tenant admin which is PG-only.
#[cfg(feature = "postgres")]
pub mod tenant_console;

#[cfg(feature = "postgres")]
pub use auth::{authenticate_operator, authenticate_user};
pub use auth::{
    authenticate_operator_pool, validate_tenant_user_schema, Operator, TenantUserModel, User,
    REQUIRED_USER_COLUMNS,
};
#[cfg(feature = "postgres")]
pub use auth_backends::ensure_api_keys_table;
pub use auth_backends::{
    create_api_key, ensure_api_keys_table_pool, ApiKeyBackend, AuthBackend, AuthError, AuthUser,
    BoxedBackend, JwtBackend, ModelBackend,
};
pub use bootstrap::{
    init_tenancy, init_tenancy_with, registry_bootstrap_migration,
    registry_bootstrap_migration_for, tenant_bootstrap_migration, tenant_bootstrap_migration_for,
    InitTenancyReport, REGISTRY_BOOTSTRAP_NAME, TENANT_BOOTSTRAP_NAME,
};
pub use middleware::{AuthenticatedUser, CurrentUser, RouterAuthExt};
// v0.38 — the PG-typed permission helpers stay PG-only re-exports;
// the tri-dialect `_pool` variants are the cross-dialect entry points.
#[cfg(feature = "postgres")]
pub use permissions::{
    assign_role, auto_create_permissions, clear_user_perm,
    ensure_tables as ensure_permission_tables, get_or_create_role, grant_role_perm, has_all_perms,
    has_any_perm, has_perm, remove_role, revoke_role_perm, set_user_perm, user_permissions,
    user_roles,
};
pub use permissions::{
    auto_create_permissions_pool, clear_user_perm_pool, ensure_tables_pool, grant_role_perm_pool,
    has_perm_pool, model_codenames, revoke_role_perm_pool, set_user_perm_pool,
    user_permissions_pool, user_roles_qs_pool,
};

pub use database_pools::{DatabaseConn, DatabasePool, DatabasePools};
pub use error::TenancyError;
#[cfg(feature = "postgres")]
pub use migrate::{migrate_registry, migrate_tenants};
pub use migrate::{migrate_registry_pool, TenantMigrationOutcome, TenantMigrationReport};
pub use org::{BackendKind, Org, StorageMode};
pub use pools::{
    DefaultTenantDb, PrewarmReport, TenantConn, TenantPool, TenantPools, TenantPoolsConfig,
};
pub use resolver::{
    ChainResolver, HeaderResolver, OrgResolver, PathPrefixResolver, PortResolver, SubdomainResolver,
};
pub use routes::RouteConfig;
pub use secrets::{
    ChainSecretsResolver, EnvSecretsResolver, LiteralSecretsResolver, SecretsError, SecretsResolver,
};
