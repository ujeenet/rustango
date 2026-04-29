//! Per-request tenant resolvers — turn an incoming HTTP request into
//! an [`Org`] from the registry.
//!
//! The headline routing mode (locked in v0.5 design) is **subdomain**:
//! `acme.app.com` → org with `host_pattern = "acme.app.com"`. Cookie
//! isolation by subdomain is the security win — tenant A's session
//! cookie is unreadable from tenant B's domain by browser policy.
//!
//! # Built-in resolvers
//!
//! | Resolver               | Matches against                 | Notes                                          |
//! |------------------------|---------------------------------|------------------------------------------------|
//! | [`SubdomainResolver`]  | `Org.host_pattern` ↔ `Host` hdr | Default — composed first in `ChainResolver::default()`. |
//! | [`PathPrefixResolver`] | `Org.path_prefix` ↔ URL path    | Opt-in — caller adds explicitly.               |
//! | [`HeaderResolver`]     | `Org.slug` ↔ user header value  | API-only deployments.                          |
//! | [`PortResolver`]       | `Org.port` ↔ incoming port      | Niche — hard-isolated tenant ports.            |
//! | [`ChainResolver`]      | tries each in order             | Operator builds the chain in `main.rs`.        |
//!
//! # `ChainResolver::default()`
//!
//! `[Subdomain, Header]`. Path-prefix is **not** in the default chain —
//! the operator opts in explicitly when they need both modes.
//!
//! # Apex (no subdomain) handling
//!
//! Bare `app.com` does **not** resolve to any tenant. The default
//! routing decision is that the apex hosts only operator UI
//! (`/operator/*` in Slice 6); other apex paths return 404. The
//! resolver returning `Ok(None)` is the signal — the caller (the
//! tenant-aware admin in Slice 4) translates that into a 404 for
//! tenant routes and bypasses the resolver entirely for `/operator`.

use async_trait::async_trait;
use http::request::Parts;
use http::HeaderName;
use crate::core::Column as _;
use crate::sql::sqlx::PgPool;
use crate::sql::Fetcher;

use super::error::TenancyError;
use super::org::Org;

/// Resolve an HTTP request to an [`Org`] from the registry.
///
/// Implementations live in `rustango-tenancy::resolver` (built-ins)
/// or in user crates (custom resolvers — JWT claims, API key
/// prefixes, geo routing, etc.).
///
/// `Ok(None)` means "no tenant matched"; the caller decides whether
/// that's a 404 or a fallthrough (e.g. the apex hits the operator
/// UI without ever calling the resolver).
#[async_trait]
pub trait OrgResolver: Send + Sync + 'static {
    /// Resolve `parts` against `registry` to find the matching tenant.
    ///
    /// # Errors
    /// Returns [`TenancyError::Driver`] for SQL failures during the
    /// lookup, [`TenancyError::Validation`] for malformed registry
    /// rows, or [`TenancyError::Resolution`] for explicit "no
    /// tenant" with diagnostic context (rare — most no-match cases
    /// return `Ok(None)`).
    async fn resolve(
        &self,
        parts: &Parts,
        registry: &PgPool,
    ) -> Result<Option<Org>, TenancyError>;
}

// ---------------- SubdomainResolver ----------------

/// Match the request's `Host` header against `Org.host_pattern`.
///
/// A request to `acme.app.com` finds the org whose `host_pattern` is
/// exactly `"acme.app.com"`. The apex (`app.com`) never matches —
/// only orgs with an explicit non-null `host_pattern` are eligible.
///
/// `apex_domain` is informational — used in error messages and as
/// part of the boot pre-flight check that warns when `apex_domain`
/// looks malformed. It does NOT auto-derive slugs from subdomains;
/// `manage create-tenant <slug>` writes the full `host_pattern` to
/// the Org row, and the resolver matches it verbatim.
pub struct SubdomainResolver {
    pub apex_domain: String,
}

impl SubdomainResolver {
    /// Construct from the apex (e.g. `"app.example.com"`). The apex
    /// is not validated here; the boot pre-flight in Slice 4 will
    /// fail-fast on missing or malformed apex.
    #[must_use]
    pub fn new(apex_domain: impl Into<String>) -> Self {
        Self {
            apex_domain: apex_domain.into(),
        }
    }
}

#[async_trait]
impl OrgResolver for SubdomainResolver {
    async fn resolve(
        &self,
        parts: &Parts,
        registry: &PgPool,
    ) -> Result<Option<Org>, TenancyError> {
        let Some(host) = host_from_parts(parts) else {
            return Ok(None);
        };
        // Apex only — no tenant resolution.
        if host == self.apex_domain {
            return Ok(None);
        }
        find_active_org_by(
            registry,
            Org::host_pattern.eq(host.to_owned()),
        )
        .await
    }
}

// ---------------- PathPrefixResolver ----------------

/// Match the request URL's first path segment against
/// `Org.path_prefix`. Opt-in — not in `ChainResolver::default()`.
///
/// A request to `app.com/acme/dashboard` matches an org with
/// `path_prefix = "/acme"`. The leading slash is required in the
/// stored value; the resolver builds the candidate as `"/<segment>"`
/// before lookup. Empty path or apex (`"/"`) returns `Ok(None)`.
pub struct PathPrefixResolver;

#[async_trait]
impl OrgResolver for PathPrefixResolver {
    async fn resolve(
        &self,
        parts: &Parts,
        registry: &PgPool,
    ) -> Result<Option<Org>, TenancyError> {
        let path = parts.uri.path();
        let Some(first) = path
            .trim_start_matches('/')
            .split('/')
            .next()
            .filter(|s| !s.is_empty())
        else {
            return Ok(None);
        };
        let candidate = format!("/{first}");
        find_active_org_by(registry, Org::path_prefix.eq(candidate)).await
    }
}

// ---------------- HeaderResolver ----------------

/// Match a user-chosen HTTP header value against `Org.slug`.
///
/// Useful for API-only deployments where every request carries an
/// explicit tenant identifier (`X-Org: acme`). The default header
/// is `X-Org`; configure via [`HeaderResolver::new`].
pub struct HeaderResolver {
    pub header_name: HeaderName,
}

impl HeaderResolver {
    /// Construct with a custom header name (case-insensitive).
    #[must_use]
    pub fn new(header_name: HeaderName) -> Self {
        Self { header_name }
    }
}

impl Default for HeaderResolver {
    /// `X-Org` is the recommended default header.
    fn default() -> Self {
        Self {
            header_name: HeaderName::from_static("x-org"),
        }
    }
}

#[async_trait]
impl OrgResolver for HeaderResolver {
    async fn resolve(
        &self,
        parts: &Parts,
        registry: &PgPool,
    ) -> Result<Option<Org>, TenancyError> {
        let Some(value) = parts.headers.get(&self.header_name) else {
            return Ok(None);
        };
        let slug = match value.to_str() {
            Ok(s) => s.trim(),
            Err(_) => return Ok(None),
        };
        if slug.is_empty() {
            return Ok(None);
        }
        find_active_org_by(registry, Org::slug.eq(slug.to_owned())).await
    }
}

// ---------------- PortResolver ----------------

/// Match the request URL's port against `Org.port`. Niche — used
/// for hard-isolated tenant ports in compliance / pen-test
/// scenarios. Most deployments don't need this and shouldn't put
/// it in their resolver chain.
pub struct PortResolver;

#[async_trait]
impl OrgResolver for PortResolver {
    async fn resolve(
        &self,
        parts: &Parts,
        registry: &PgPool,
    ) -> Result<Option<Org>, TenancyError> {
        let Some(port) = parts.uri.port_u16() else {
            return Ok(None);
        };
        find_active_org_by(registry, Org::port.eq(i32::from(port))).await
    }
}

// ---------------- ChainResolver ----------------

/// Try each resolver in order; first `Ok(Some(_))` wins. `Ok(None)`
/// from one resolver falls through to the next; an `Err` short-
/// circuits (the caller usually surfaces it as 500).
///
/// Default: `[SubdomainResolver, HeaderResolver]` — subdomain-first
/// per the v0.5 design, with `X-Org` as a fallback for API clients.
/// `PathPrefixResolver` is **not** in the default chain — operators
/// add it explicitly when they need path-based routing too.
pub struct ChainResolver {
    resolvers: Vec<Box<dyn OrgResolver>>,
}

impl ChainResolver {
    /// Empty chain. Use [`ChainResolver::push`] to add resolvers.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resolvers: Vec::new(),
        }
    }

    /// Append a resolver to the chain. Returns `self` for builder
    /// ergonomics.
    #[must_use]
    pub fn push<R: OrgResolver>(mut self, resolver: R) -> Self {
        self.resolvers.push(Box::new(resolver));
        self
    }

    /// Standard chain: subdomain first, then `X-Org` header. The
    /// `apex_domain` is the bare app domain (e.g. `"app.example.com"`)
    /// — `app.com` itself never resolves to a tenant; only its
    /// subdomains do.
    #[must_use]
    pub fn standard(apex_domain: impl Into<String>) -> Self {
        Self::new()
            .push(SubdomainResolver::new(apex_domain))
            .push(HeaderResolver::default())
    }
}

impl Default for ChainResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OrgResolver for ChainResolver {
    async fn resolve(
        &self,
        parts: &Parts,
        registry: &PgPool,
    ) -> Result<Option<Org>, TenancyError> {
        for resolver in &self.resolvers {
            match resolver.resolve(parts, registry).await? {
                Some(org) => return Ok(Some(org)),
                None => continue,
            }
        }
        Ok(None)
    }
}

// ---------------- helpers ----------------

/// Pull the host name (no port, no scheme) from the request. Tries
/// `Host` header first (universal), falls back to `parts.uri.host()`
/// for clients that send absolute-form URIs.
fn host_from_parts(parts: &Parts) -> Option<&str> {
    if let Some(value) = parts.headers.get(http::header::HOST) {
        if let Ok(s) = value.to_str() {
            // `Host` header may include `:port` — strip it.
            return Some(s.split(':').next().unwrap_or(s));
        }
    }
    parts.uri.host()
}

/// Run `Org::objects().where_(filter).where_(active=true)` and
/// return the first match. Helper extracted because every resolver
/// shape ends in this same query.
async fn find_active_org_by<F>(
    registry: &PgPool,
    filter: F,
) -> Result<Option<Org>, TenancyError>
where
    F: Into<rustango::core::TypedFilter<Org>>,
{
    let typed: rustango::core::TypedFilter<Org> = filter.into();
    let rows: Vec<Org> = Org::objects()
        .where_(typed)
        .where_(Org::active.eq(true))
        .fetch(registry)
        .await
        .map_err(|e| TenancyError::Driver(driver_from_exec(e)))?;
    Ok(rows.into_iter().next())
}

/// Convert an `ExecError` into the `sqlx::Error` shape `TenancyError`
/// stores. We don't want `TenancyError::Driver` to wrap the full
/// `ExecError` because it carries `QueryError`/`SqlError` shapes
/// that don't apply to resolver lookups; we surface only the
/// driver-level cause.
fn driver_from_exec(e: rustango::sql::ExecError) -> rustango::sql::sqlx::Error {
    use crate::sql::ExecError;
    match e {
        ExecError::Driver(err) => err,
        // Resolver queries are simple `where_(... eq ...)` lookups
        // — Query/Sql shape errors here would be a rustango bug,
        // not a user error. Wrap as a synthetic driver error so
        // callers don't need to match every ExecError variant.
        other => rustango::sql::sqlx::Error::Protocol(format!("resolver query: {other}")),
    }
}
