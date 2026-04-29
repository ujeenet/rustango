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
//! [`TenantPools`]: ()  // FIXME: link when slice 3 lands.
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

mod error;
mod org;
mod resolver;

pub use error::TenancyError;
pub use org::{Org, StorageMode};
pub use resolver::{
    ChainResolver, HeaderResolver, OrgResolver, PathPrefixResolver, PortResolver,
    SubdomainResolver,
};
