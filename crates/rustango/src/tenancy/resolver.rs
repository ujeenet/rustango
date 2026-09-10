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

use crate::core::Column as _;
use crate::sql::FetcherPool;
use crate::sql::Pool;
use async_trait::async_trait;
use http::request::Parts;
use http::HeaderName;

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
    async fn resolve(&self, parts: &Parts, registry: &Pool) -> Result<Option<Org>, TenancyError>;
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
/// `cargo run -- create-tenant <slug>` writes the full `host_pattern`
/// to the Org row, and the resolver matches it verbatim.
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
    async fn resolve(&self, parts: &Parts, registry: &Pool) -> Result<Option<Org>, TenancyError> {
        let Some(host) = host_from_parts(parts) else {
            return Ok(None);
        };
        // Apex only — no tenant resolution.
        if host == self.apex_domain {
            return Ok(None);
        }
        find_active_org_by(registry, Org::host_pattern.eq(host.to_owned())).await
    }
}

// ---------------- RegisteredHostResolver ----------------

/// Match the `Host` header against an extra hostname registered in
/// [`rustango_org_hosts`](super::OrgHost), so one tenant can serve several
/// domains beyond its base `Org.host_pattern`.
///
/// Composed AFTER [`SubdomainResolver`] in the standard chain: the base
/// host keeps resolving exactly as it always did, and this only ever runs
/// on a host that would otherwise have found nothing. The feature is
/// purely additive — it cannot change where an existing request lands.
///
/// ## Missing table is not an error
///
/// The `rustango_org_hosts` table arrives with the generated system
/// migration chain, so a deployment that upgrades the binary and has not
/// yet run `migrate` does not have it. Propagating that error would turn
/// "you haven't migrated yet" into a 500 on **every** request, including
/// for tenants that never use extra hosts. So a failed lookup is logged
/// once and treated as "no match", which is the pre-upgrade behaviour to
/// the byte.
pub struct RegisteredHostResolver;

/// Ensures the missing-table warning is logged once per process rather
/// than once per request.
static HOST_TABLE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// hostname → resolved org id (`None` = known-miss), with an expiry.
///
/// Tenant resolution is otherwise uncached — every request already pays one
/// registry SELECT — so this exists to stop the *extra* lookup this
/// resolver adds becoming a per-request cost.
///
/// Negative entries matter more than positive ones. The chain
/// short-circuits, so a request to a tenant's base host never reaches this
/// resolver at all; what does reach it is every request to a host nobody
/// has registered. Without a negative cache, spraying random `Host` headers
/// is a free registry query per request.
///
/// Bounded on purpose: the keys are attacker-supplied, so an unbounded map
/// would trade a query amplification for a memory one. At the cap the whole
/// map is dropped — crude, but O(1) and always correct for a cache.
///
/// Keyed by hostname alone, not (registry, hostname): a process serves one
/// registry. Tests that stand up several must use distinct hostnames.
type HostCacheMap = std::collections::HashMap<String, (Option<i64>, std::time::Instant)>;
static HOST_CACHE: std::sync::RwLock<Option<HostCacheMap>> = std::sync::RwLock::new(None);
const HOST_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const HOST_CACHE_MAX: usize = 1024;

/// The three states a lookup can be in. Named rather than
/// `Option<Option<i64>>` because "not cached" and "cached as a miss" lead
/// to opposite actions — one queries, the other must not.
enum Cached {
    /// Nothing usable; go to the registry.
    Absent,
    /// Known not to be a registered host.
    Miss,
    Hit(i64),
}

fn host_cache_get(host: &str) -> Cached {
    let Ok(guard) = HOST_CACHE.read() else {
        return Cached::Absent;
    };
    let Some((value, at)) = guard.as_ref().and_then(|m| m.get(host)) else {
        return Cached::Absent;
    };
    if at.elapsed() > HOST_CACHE_TTL {
        return Cached::Absent;
    }
    match value {
        Some(id) => Cached::Hit(*id),
        None => Cached::Miss,
    }
}

fn host_cache_put(host: &str, value: Option<i64>) {
    let Ok(mut guard) = HOST_CACHE.write() else {
        return;
    };
    let map = guard.get_or_insert_with(Default::default);
    if map.len() >= HOST_CACHE_MAX {
        map.clear();
    }
    map.insert(host.to_owned(), (value, std::time::Instant::now()));
}

/// Last host-table fingerprint this process saw, and when it last looked.
///
/// [`invalidate_host_cache`] only clears the process that called it. Behind
/// a load balancer that is half a solution: the pod handling the admin
/// request forgets, and every other pod keeps answering from a cache that
/// is now wrong. Polling a cheap fingerprint closes that gap without a
/// shared cache, a message bus, or making Redis a dependency of tenant
/// resolution — the one path that runs before everything else and must not
/// acquire new ways to fail.
/// `None` = never looked. `Some((gen, at))` carries the last fingerprint
/// seen and the time of the last *attempt* — successful or not, so a
/// failing probe is still throttled.
///
/// `gen` is itself `Option`: a claimed-but-not-yet-completed check writes
/// the attempt time with the previous fingerprint, so a probe that fails
/// leaves the last known-good value in place rather than resetting it.
type GenState = (Option<super::org_host::Generation>, std::time::Instant);

static HOST_GEN: std::sync::RwLock<Option<GenState>> = std::sync::RwLock::new(None);

/// Ensures a failing fingerprint probe is logged once per process rather
/// than once per interval. Without this the mechanism can be dead for a
/// process's entire lifetime with nothing in the logs to say so.
static HOST_GEN_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Default interval between fingerprint reads — the bound on how long
/// another pod can serve a stale answer.
const GEN_CHECK_EVERY_DEFAULT: std::time::Duration = std::time::Duration::from_secs(5);

/// Env override for [`GEN_CHECK_EVERY_DEFAULT`], in seconds. `0` disables
/// cross-process polling entirely.
///
/// A hard-coded constant is the wrong shape for this: a single-process
/// deployment gains nothing from the poll, a cross-region registry wants a
/// longer interval, and an operator who needs tighter convergence wants a
/// shorter one. None of them should have to fork the crate, and this runs
/// on the path that must not acquire new ways to fail.
const GEN_CHECK_ENV: &str = "RUSTANGO_HOST_GEN_CHECK_SECS";

/// Resolved once — reading and parsing an env var on every resolve would
/// cost more than the check it is gating.
fn gen_check_every() -> Option<std::time::Duration> {
    static CACHED: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| match std::env::var(GEN_CHECK_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(std::time::Duration::from_secs(secs)),
            Err(_) => {
                tracing::warn!(
                    target: "rustango::tenancy::resolver",
                    value = %raw,
                    "{GEN_CHECK_ENV} is not a whole number of seconds; \
                     falling back to the default interval"
                );
                Some(GEN_CHECK_EVERY_DEFAULT)
            }
        },
        Err(_) => Some(GEN_CHECK_EVERY_DEFAULT),
    })
}

/// Drop the local cache if another process changed the host table.
///
/// Cheap to call on every resolve: it does nothing at all until
/// [`GEN_CHECK_EVERY`] has passed.
async fn sync_generation(registry: &Pool) {
    let Some(interval) = gen_check_every() else {
        return; // polling disabled
    };

    // CLAIM the check before awaiting, in one write-lock section.
    //
    // Two bugs live in the alternative — stamping the time only after a
    // successful probe. A probe that fails would leave the timestamp
    // unadvanced, so every subsequent request re-probes forever: a pod
    // deployed before `migrate` ran, or riding out a registry blip, turns
    // a 5s poll into a per-request query storm. And under concurrency
    // every request arriving while a probe is in flight would also see
    // "due" and fire its own, fanning out exactly when the registry is
    // already slow.
    //
    // Claiming both throttles failures and dedups in-flight checks: the
    // first caller through takes the slot, everyone else sees a fresh
    // timestamp and returns immediately.
    //
    // The lock is released before the await — holding a std RwLock across
    // one parks it on whatever task resumes and can deadlock the next
    // reader on the same thread.
    let previous = {
        match HOST_GEN.write() {
            Ok(mut g) => {
                let due = g.is_none_or(|(_, at)| at.elapsed() >= interval);
                if !due {
                    return;
                }
                let previous = g.and_then(|(seen, _)| seen);
                *g = Some((previous, std::time::Instant::now()));
                previous
            }
            // A poisoned lock means some other thread panicked mid-update.
            // Skip this round rather than resolving off a torn value.
            Err(_) => return,
        }
    };

    let current = match super::org_host::generation(registry).await {
        Ok(current) => current,
        Err(e) => {
            // Never fail a request over a cache refresh — but do not fail
            // silently either. Cross-pod invalidation being dead is
            // invisible from the outside until a host 404s on some pods
            // and not others, which is near-impossible to correlate after
            // the fact.
            if !HOST_GEN_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    target: "rustango::tenancy::resolver",
                    error = %e,
                    "could not read the rustango_org_hosts fingerprint; this pod \
                     will not notice host changes made by other processes until \
                     its own cache entries expire"
                );
            }
            // The claim above already recorded the attempt, so this backs
            // off for a full interval instead of retrying every request.
            return;
        }
    };

    let changed = {
        match HOST_GEN.write() {
            Ok(mut g) => {
                *g = Some((Some(current), std::time::Instant::now()));
                previous.is_some_and(|seen| seen != current)
            }
            Err(_) => false,
        }
    };
    if changed {
        // Clears negative entries too, and must: a hostname cached as a
        // known-miss is exactly what an add on another pod invalidates.
        // The cost is that a registry with a steady write stream keeps
        // every pod's negative cache short-lived, which is the deliberate
        // trade — a stale 404 on a real customer domain is worse than a
        // repeated lookup on a sprayed one, and `HOST_CACHE_MAX` still
        // bounds the latter.
        invalidate_host_cache();
    }
}

/// Forget the generation state entirely.
///
/// `pub(crate)` and surfaced through [`crate::testkit`] rather than being
/// `pub` here: as public API of `rustango::tenancy` these would be
/// permanent semver surface that any downstream crate could call in
/// production, silently defeating cross-process invalidation for the life
/// of the process. `#[doc(hidden)]` hides a name from rustdoc, not from
/// the compiler.
#[cfg(any(test, feature = "testkit"))]
pub(crate) fn reset_generation() {
    if let Ok(mut g) = HOST_GEN.write() {
        *g = None;
    }
}

/// Make the next resolve re-read the fingerprint immediately instead of
/// waiting out the poll interval, so a cross-pod test proves the bound
/// without sleeping through it.
///
/// Unconditional by design. Guarding this on an existing `Some` made it a
/// silent no-op in precisely the case a test reaches for it — right after
/// a reset — so the test read as though it forced a re-check while
/// actually doing nothing.
#[cfg(any(test, feature = "testkit"))]
pub(crate) fn expire_generation() {
    if let Ok(mut g) = HOST_GEN.write() {
        let previous = g.and_then(|(seen, _)| seen);
        // Back-date past the *effective* interval, not the default one —
        // with a longer interval configured, subtracting the default
        // would leave the entry un-due and make this hook a no-op again.
        let interval = gen_check_every().unwrap_or(GEN_CHECK_EVERY_DEFAULT);
        // `checked_sub` returns None when the monotonic clock is younger
        // than the offset — reachable on a freshly booted host. Falling
        // back to `now()` there would *unexpire* the entry and invert this
        // function's whole purpose, so fall back to the process's own
        // epoch instead, which is unambiguously "long ago".
        let long_ago = std::time::Instant::now()
            .checked_sub(interval * 2)
            .unwrap_or(*PROCESS_START);
        *g = Some((previous, long_ago));
    }
}

/// Earliest `Instant` this process can name. Used as a saturating floor
/// by [`expire_generation`].
#[cfg(any(test, feature = "testkit"))]
static PROCESS_START: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// Drop cached resolutions. Called after a host is bound, unbound or
/// toggled so the admin's next request reflects the change instead of
/// waiting out the TTL.
///
/// Clears everything rather than one key: a rename is a remove plus an add,
/// and the map is small and cheap to refill.
pub fn invalidate_host_cache() {
    if let Ok(mut guard) = HOST_CACHE.write() {
        if let Some(map) = guard.as_mut() {
            map.clear();
        }
    }
}

#[async_trait]
impl OrgResolver for RegisteredHostResolver {
    async fn resolve(&self, parts: &Parts, registry: &Pool) -> Result<Option<Org>, TenancyError> {
        let Some(host) = host_from_parts(parts) else {
            return Ok(None);
        };
        // Pick up a host added or toggled by another pod before trusting
        // anything cached here. Throttled — see `GEN_CHECK_EVERY`.
        sync_generation(registry).await;
        // Cached, including the miss — see `HOST_CACHE`.
        match host_cache_get(host) {
            Cached::Miss => return Ok(None),
            Cached::Hit(org_id) => return find_active_org_by(registry, Org::id.eq(org_id)).await,
            Cached::Absent => {}
        }
        let rows = match super::OrgHost::objects()
            .where_(super::OrgHost::hostname.eq(host.to_owned()))
            .where_(super::OrgHost::enabled.eq(true))
            .fetch(registry)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                if !HOST_TABLE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        target: "rustango::tenancy::resolver",
                        error = %e,
                        "could not read rustango_org_hosts; extra tenant hostnames \
                         are inactive until `migrate` creates the table"
                    );
                }
                return Ok(None);
            }
        };
        let Some(row) = rows.into_iter().next() else {
            host_cache_put(host, None);
            return Ok(None);
        };
        host_cache_put(host, Some(row.org_id));
        find_active_org_by(registry, Org::id.eq(row.org_id)).await
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
    async fn resolve(&self, parts: &Parts, registry: &Pool) -> Result<Option<Org>, TenancyError> {
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
///
/// # Security
///
/// The header value comes straight from the client. Without an
/// [`Self::allow_only`] allowlist, an attacker who reaches the
/// resolver chain can request *any* tenant by name; defense-in-depth
/// against IDOR therefore depends on every downstream handler
/// checking `Tenant`-scoped authorization. For API-key or
/// JWT-authenticated deployments where the credential is itself
/// tenant-scoped this is fine; for ambient-cookie deployments,
/// always pair this resolver with `allow_only`.
pub struct HeaderResolver {
    pub header_name: HeaderName,
    allowed_slugs: Option<std::collections::HashSet<String>>,
}

impl HeaderResolver {
    /// Construct with a custom header name (case-insensitive).
    #[must_use]
    pub fn new(header_name: HeaderName) -> Self {
        Self {
            header_name,
            allowed_slugs: None,
        }
    }

    /// Restrict accepted header values to a fixed allowlist of slugs.
    /// Requests whose header value is not in the set resolve to
    /// `None` (the chain falls through to the next resolver, then to
    /// the operator-console fallback if nothing matches). v0.43.
    ///
    /// ```ignore
    /// HeaderResolver::default().allow_only(["acme", "globex"])
    /// ```
    #[must_use]
    pub fn allow_only<I, S>(mut self, slugs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_slugs = Some(slugs.into_iter().map(Into::into).collect());
        self
    }
}

impl Default for HeaderResolver {
    /// `X-Org` is the recommended default header.
    fn default() -> Self {
        Self {
            header_name: HeaderName::from_static("x-org"),
            allowed_slugs: None,
        }
    }
}

#[async_trait]
impl OrgResolver for HeaderResolver {
    async fn resolve(&self, parts: &Parts, registry: &Pool) -> Result<Option<Org>, TenancyError> {
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
        // v0.43 — allowlist short-circuit, before the DB lookup.
        if let Some(allowed) = &self.allowed_slugs {
            if !allowed.contains(slug) {
                return Ok(None);
            }
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
    async fn resolve(&self, parts: &Parts, registry: &Pool) -> Result<Option<Org>, TenancyError> {
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
            // After the base host, before the header fallback: an extra
            // hostname must never outrank a tenant's own `host_pattern`,
            // and until an operator registers one the table is empty, so
            // no existing deployment changes behaviour.
            .push(RegisteredHostResolver)
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
    async fn resolve(&self, parts: &Parts, registry: &Pool) -> Result<Option<Org>, TenancyError> {
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
async fn find_active_org_by<F>(registry: &Pool, filter: F) -> Result<Option<Org>, TenancyError>
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

#[cfg(test)]
mod tests {
    use super::*;

    // v0.43 — lock in the HeaderResolver allowlist behaviour at the
    // builder level. The full resolve() path requires a live Pool +
    // Org row; that's exercised in the tenant_extractor_*_live tests.
    // These unit tests guarantee the builder semantics so a refactor
    // can't silently drop the allowlist set.

    #[test]
    fn default_has_no_allowlist() {
        let r = HeaderResolver::default();
        assert!(
            r.allowed_slugs.is_none(),
            "default resolver should not have an allowlist"
        );
    }

    #[test]
    fn allow_only_stores_slugs() {
        let r = HeaderResolver::default().allow_only(["acme", "globex"]);
        let set = r.allowed_slugs.expect("allow_only sets the slug set");
        assert_eq!(set.len(), 2);
        assert!(set.contains("acme"));
        assert!(set.contains("globex"));
        assert!(!set.contains("attacker-tenant"));
    }

    #[test]
    fn allow_only_accepts_owned_strings() {
        // `Into<String>` bound — verify Vec<String> also works.
        let slugs: Vec<String> = vec!["a".into(), "b".into()];
        let r = HeaderResolver::default().allow_only(slugs);
        assert_eq!(
            r.allowed_slugs.expect("allow_only sets the slug set").len(),
            2
        );
    }
}
