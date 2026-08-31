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
//! ## Design choices (locked 2026-04-28; revisited 2026-05-12)
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
//! 7. **Two storage modes, one universal + one PG optimization**
//!    (v0.38). Database-mode (one dedicated DB/file per tenant)
//!    works on every backend — PostgreSQL, MySQL, and SQLite — and
//!    is the right default for enterprise B2B, compliance-sensitive
//!    deployments, and anything with fewer than a few hundred
//!    tenants. Schema-mode (many tenants share one PG database;
//!    isolated by `SET search_path`) is a Postgres-only
//!    optimization for high-N-low-revenue SaaS where connection
//!    counts or per-tenant DB overhead actually matter. On MySQL
//!    and SQLite, `Org.storage_mode = "schema"` returns a clear
//!    runtime validation error pointing the user at database-mode;
//!    isolation semantics are equivalent.
//!
//! See `memory/v05-multitenancy-roadmap.md` in the project memory for
//! the full design and slice plan.

// v0.38 — `tenancy::admin` is tri-dialect: handler bodies route
// through the ORM `_pool` family + the unified `crate::sql::Pool`
// enum. Tenants in database-mode work on PG / MySQL / SQLite. The
// schema-mode dispatch (where many tenants share one PG database
// via `SET search_path`) is a Postgres-only optimization; on non-PG
// backends, a schema-mode org returns `TenancyError::Validation`
// from `TenantPools<DB>::scoped_pool_dyn` with a message pointing
// the user at database-mode.
#[cfg(feature = "admin")]
pub mod admin;
// Agents + skills are an MCP-only concept (the grant unit for tool
// authorization). Gated behind `mcp` so non-MCP tenancy projects don't
// compile the models, register them for `makemigrations`, or carry their
// tables in the bootstrap. Enabling `mcp` makes `makemigrations` emit the
// `CreateModel`; disabling it emits the `DropModel`.
#[cfg(feature = "mcp")]
pub mod agents;
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
/// Member (end-user) social SSO — OpenID Connect / social OAuth login
/// for a tenant's own user pool, with optional auto-provisioning.
/// Reuses the admin-independent `crate::sso` core + the DB-backed
/// `SsoProvider` rows — builds with `tenancy + sso`, no admin required.
#[cfg(all(feature = "tenancy", feature = "sso"))]
pub mod member_auth;
// v0.45 (#253) — `manage_interactive` was promoted to the crate
// root so the bare admin's `create-admin` verb (slice B) can reuse
// the same TTY-gated prompt helpers. Re-export keeps existing
// tenancy callers (`tenants.rs`, `users.rs`) working.
pub(crate) use crate::manage_interactive;
pub mod middleware;
pub mod migrate;
pub mod operator_console;
mod org;
pub mod password;
pub mod permissions;
mod pools;
/// Who a request is acting as, whatever authenticated it (session cookie,
/// Bearer access token, MCP agent token).
pub mod principal;
mod resolver;
pub mod routes;
mod secrets;
pub mod session;
/// Per-`Org` SSO (OpenID Connect / social OAuth) login for the tenant
/// admin — the `admin-sso` feature. Reuses `crate::admin::sso`.
#[cfg(feature = "admin-sso")]
pub(crate) mod sso;
pub mod sweep;
// v0.38 slice 24 — `tenancy::server::run<DB>` is tri-dialect; the
// operator console runs on whichever backend `TenantPools<DB>` was
// built with, schema-mode dispatch only fires on PG (by language),
// and the inner admin uses the unified `crate::sql::Pool` enum.
pub mod server;
pub mod tenant_console;

#[cfg(feature = "mcp")]
pub use agents::{
    add_skill_resource_pool, agent_token_still_valid_pool, authenticate_agent_by_prefix_pool,
    authenticate_agent_pool, create_agent_pool, create_skill_pool, create_user_key_pool,
    delete_user_keys_pool, grant_skill_pool, list_agents_pool, list_skills_pool,
    list_user_keys_pool, map_skill_to_permission_pool, resolve_agent_grants_pool,
    resolve_user_agent_grants_pool, resources_for_skills_pool, revoke_skill_pool,
    revoke_user_key_pool, rotate_agent_secret_pool, skills_by_codenames_pool,
    unmap_skill_from_permission_pool, Agent, AgentError, AgentGrant, AgentSecret, AgentSkill,
    AgentSkillPermission, AgentSkillResource, AgentSkillTool,
};
#[cfg(feature = "postgres")]
pub use auth::{authenticate_operator, authenticate_user};
pub use auth::{
    authenticate_operator_pool, authenticate_user_pool, validate_tenant_user_schema, Operator,
    TenantUserModel, User, REQUIRED_USER_COLUMNS,
};
#[cfg(feature = "postgres")]
pub use auth_backends::ensure_api_keys_table;
pub use auth_backends::{
    create_api_key, ensure_api_keys_table_pool, ApiKeyBackend, AuthBackend, AuthError, AuthUser,
    BoxedBackend, JwtBackend, ModelBackend,
};
pub use bootstrap::{init_tenancy, init_tenancy_with, InitTenancyReport};
pub use middleware::{AuthenticatedUser, CurrentUser, RouterAuthExt};
pub use principal::{OptionalPrincipal, Principal, PrincipalKind, TenantSlug};
// v0.38 — the PG-typed permission helpers stay PG-only re-exports;
// the tri-dialect `_pool` variants are the cross-dialect entry points.
#[cfg(feature = "postgres")]
pub use permissions::{
    assign_role, auto_create_permissions, clear_user_perm, get_or_create_role, grant_role_perm,
    has_all_perms, has_any_perm, has_perm, remove_role, revoke_role_perm, set_user_perm,
    user_permissions, user_roles,
};
pub use permissions::{
    auto_create_permissions_pool, clear_user_perm_pool, create_role_pool, ensure_tables_pool,
    get_or_create_role_pool, grant_role_perm_pool, has_all_perms_pool, has_any_perm_pool,
    has_perm_pool, model_codenames, revoke_role_perm_pool, set_user_perm_pool,
    user_permissions_pool, user_roles_pool, user_roles_qs_pool,
};

pub use database_pools::{DatabaseConn, DatabasePool, DatabasePools};
pub use error::TenancyError;
#[cfg(all(feature = "tenancy", feature = "sso"))]
pub use member_auth::{member_sso_router, CurrentMember, MemberAuthConfig, MEMBER_COOKIE};
#[cfg(feature = "postgres")]
pub use migrate::migrate_tenants;
pub use migrate::{
    migrate_registry, migrate_registry_pool, migrate_tenants_db, migrate_tenants_dyn,
    TenantMigrationOutcome, TenantMigrationReport,
};
pub use org::{BackendKind, Org, StorageMode};
pub use pools::{
    DefaultTenantDb, PrewarmReport, TenantConn, TenantPool, TenantPoolInvalidator, TenantPools,
    TenantPoolsConfig,
};
pub use resolver::{
    ChainResolver, HeaderResolver, OrgResolver, PathPrefixResolver, PortResolver, SubdomainResolver,
};
pub use routes::RouteConfig;
pub use secrets::{
    ChainSecretsResolver, EnvSecretsResolver, LiteralSecretsResolver, SecretsError, SecretsResolver,
};
pub use sweep::{active_tenants, for_each_tenant, SweepError, TenantOutcome, TenantSweep};
