//! SSO (OpenID Connect / social OAuth) login — the admin-INDEPENDENT
//! `sso` feature.
//!
//! This module is the **shared core** used by every SSO surface: the
//! bare admin, the tenant admin console ([`crate::tenancy::sso`]), and
//! the member / end-user flow ([`crate::tenancy::member_auth`]). It has
//! no dependency on the auto-admin, so member SSO can build without
//! pulling in `crate::admin`. Providers are DB rows managed from the
//! admin UI (the [`SsoProvider`] / [`crate::tenancy::sso::SharedSsoProvider`]
//! models), not config — each surface loads its enabled providers and
//! builds one [`OAuth2Provider`] per login.
//!
//! It reuses the existing [`crate::oauth2`] handshake
//! ([`OAuth2Provider::begin`]/[`complete`](crate::oauth2::OAuth2Provider::complete) +
//! [`seal_flow`]/[`open_flow`]) to prove identity, then signs in the user
//! linked to the IdP subject ([`link`]) and mints that surface's normal
//! session cookie. SSO never auto-provisions an admin. (The member flow
//! may opt into auto-provisioning.)
//!
//! The client secret is stored encrypted in the provider row and decrypted
//! only when the provider is built for a login.
//!
//! [`SsoProvider`]: crate::sso::SsoProvider
//! [`OAuth2Provider`]: crate::oauth2::OAuth2Provider
//! [`OAuth2Provider::begin`]: crate::oauth2::OAuth2Provider::begin
//! [`seal_flow`]: crate::oauth2::seal_flow
//! [`open_flow`]: crate::oauth2::open_flow

#[cfg(any(feature = "tenancy", feature = "admin-sso"))]
pub(crate) mod check;
/// Password rules and idle deactivation.
#[cfg(any(feature = "tenancy", feature = "admin"))]
pub mod lifecycle;
pub mod link;
pub mod policy;
pub mod provider;
pub use link::{LinkSource, ProviderKey, SsoLink, StaleLinks};
pub use provider::{list_enabled, resolve_by_slug, ResolvedProvider, SsoProvider};

use crate::oauth2::{providers, ClientConfig, OAuth2Provider, OAuthError};
use crate::outbound::TargetPolicy;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

pub use crate::oauth2::{open_flow, seal_flow, FlowPurpose, FlowScope, NormalizedUser, OAuth2Flow};

/// Cookie the sealed [`OAuth2Flow`] round-trips in between the login
/// redirect and the callback. Distinct from the standalone
/// `oauth2::router`'s `rustango_oauth_flow` so the two can coexist.
pub const SSO_FLOW_COOKIE: &str = "rustango_admin_sso_flow";

/// One surface's flow cookie at one cookie `Path`. A host-only cookie with
/// `Path=/` over HTTPS takes the `__Host-` prefix, so a sibling subdomain
/// cannot plant or shadow it; a prefix tenant (`Path=/acme`) cannot.
#[cfg(feature = "admin")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct FlowCookie<'a> {
    name: &'static str,
    path: &'a str,
    secure: bool,
}

#[cfg(feature = "admin")]
impl<'a> FlowCookie<'a> {
    pub(crate) fn new(name: &'static str, path: &'a str, secure: bool) -> Self {
        Self { name, path, secure }
    }

    fn host_only(self) -> bool {
        self.secure && self.path == "/"
    }

    /// The name this cookie is set under.
    pub(crate) fn name(self) -> String {
        if self.host_only() {
            format!("__Host-{}", self.name)
        } else {
            self.name.to_owned()
        }
    }

    fn attrs(self, max_age: i64) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!(
            "; Path={}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
            self.path
        )
    }

    /// The `Set-Cookie` value holding `sealed`.
    pub(crate) fn set(self, sealed: &str, max_age: i64) -> String {
        format!("{}={sealed}{}", self.name(), self.attrs(max_age))
    }

    /// `Set-Cookie` values that expire it, under both names.
    pub(crate) fn clear(self) -> Vec<String> {
        let mut out = vec![format!("{}={}", self.name(), self.attrs(0))];
        if self.host_only() {
            out.push(format!("{}={}", self.name, self.attrs(0)));
        }
        out
    }

    /// The sealed flow from the request, under [`Self::name`] only: a plain
    /// name beside `__Host-` could be planted from a sibling subdomain.
    pub(crate) fn read(self, headers: &axum::http::HeaderMap) -> Option<&str> {
        crate::cookies::cookie_from_headers(headers, &self.name())
    }
}

#[cfg(feature = "admin")]
/// Flows already completed in this process, oldest first, kept as long as
/// a flow can live.
struct UsedFlows {
    order: std::collections::VecDeque<(Instant, String)>,
    seen: std::collections::HashSet<String>,
}

#[cfg(feature = "admin")]
static USED_FLOWS: LazyLock<Mutex<UsedFlows>> = LazyLock::new(|| {
    Mutex::new(UsedFlows {
        order: std::collections::VecDeque::new(),
        seen: std::collections::HashSet::new(),
    })
});

#[cfg(feature = "admin")]
/// Most completed flows remembered; past it the oldest is forgotten.
const USED_FLOWS_MAX: usize = 100_000;

#[cfg(feature = "admin")]
/// Claim `flow` for its one callback: `false` when this process already
/// took it. Per process; across replicas the IdP's one-time code still holds.
pub(crate) fn claim_flow(flow: &OAuth2Flow) -> bool {
    let keep = Duration::from_secs(crate::oauth2::MAX_FLOW_AGE_SECS + 60);
    let mut used = USED_FLOWS.lock().unwrap_or_else(PoisonError::into_inner);
    while let Some((at, state)) = used.order.front().cloned() {
        if at.elapsed() < keep && used.order.len() < USED_FLOWS_MAX {
            break;
        }
        used.order.pop_front();
        used.seen.remove(&state);
    }
    if !used.seen.insert(flow.state.clone()) {
        return false;
    }
    used.order.push_back((Instant::now(), flow.state.clone()));
    true
}

/// A fully-resolved SSO provider config — the client secret is the
/// decrypted value, ready to build a provider. Both surfaces normalize their stored config into this.
#[derive(Clone)]
pub struct ResolvedSso {
    /// Provider key: `"google"`, `"microsoft"`, `"github"`, `"gitlab"`,
    /// `"discord"`, or `"oidc"` for a generic OpenID Connect provider.
    pub provider: String,
    /// OIDC issuer base URL — required when `provider == "oidc"`.
    pub issuer_url: Option<String>,
    pub client_id: String,
    /// Decrypted secret value.
    pub client_secret: String,
    /// Must match the route mounted at `<login>/sso/{provider}/callback`.
    pub redirect_uri: String,
    /// Optional OAuth scope override. `None` keeps the provider defaults
    /// (`openid email profile`); `Some(list)` replaces them.
    pub scopes: Option<Vec<String>>,
}

/// Redacted: `{:?}` must not print the client secret.
impl std::fmt::Debug for ResolvedSso {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSso")
            .field("provider", &self.provider)
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Errors surfaced by the SSO login flow. Rendered as a generic
/// user-facing message by the handlers — details go to `tracing`.
#[derive(Debug)]
pub enum SsoError {
    /// `provider` key isn't a known preset or `"oidc"`.
    UnknownProvider(String),
    /// `provider == "oidc"` but no `issuer_url` was configured.
    MissingIssuer,
    /// SSO isn't enabled for this surface / tenant.
    NotEnabled,
    /// The IdP returned an unverified email — refused.
    EmailNotVerified,
    /// No admin user matches the verified email (link-to-existing).
    NoMatchingUser(String),
    /// The matched user is inactive.
    Inactive,
    /// Client-secret reference could not be resolved.
    Secret(String),
    /// Misconfiguration (missing client_id, bad redirect, …).
    Config(String),
    /// Underlying OAuth2/OIDC handshake error.
    Oauth(OAuthError),
}

impl From<OAuthError> for SsoError {
    fn from(e: OAuthError) -> Self {
        SsoError::Oauth(e)
    }
}

impl std::fmt::Display for SsoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SsoError::UnknownProvider(p) => write!(f, "unknown SSO provider: {p}"),
            SsoError::MissingIssuer => write!(f, "provider=oidc requires an issuer_url"),
            SsoError::NotEnabled => write!(f, "SSO is not enabled"),
            SsoError::EmailNotVerified => write!(f, "the IdP email is not verified"),
            SsoError::NoMatchingUser(e) => write!(f, "no admin account for {e}"),
            SsoError::Inactive => write!(f, "the matched account is inactive"),
            SsoError::Secret(m) => write!(f, "could not resolve the SSO client secret: {m}"),
            SsoError::Config(m) => write!(f, "SSO misconfigured: {m}"),
            SsoError::Oauth(e) => write!(f, "SSO handshake failed: {e}"),
        }
    }
}

impl std::error::Error for SsoError {}

/// Check a provider key and issuer before a config is saved, as
/// [`build_provider`] would at login.
#[cfg_attr(not(all(feature = "tenancy", feature = "admin-sso")), allow(dead_code))]
pub(crate) fn check_provider(provider: &str, issuer_url: Option<&str>) -> Result<(), SsoError> {
    match provider {
        "google" | "microsoft" | "github" | "gitlab" | "discord" => Ok(()),
        "oidc" if issuer_url.is_some_and(|u| !u.trim().is_empty()) => Ok(()),
        "oidc" => Err(SsoError::MissingIssuer),
        other => Err(SsoError::UnknownProvider(other.to_owned())),
    }
}

/// `http://` to a host other than loopback.
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) fn insecure_url(url: &str) -> bool {
    let url = url.trim();
    let Some(rest) = url
        .get(..7)
        .filter(|s| s.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    !crate::urls::is_loopback_host(host)
}

/// The provider row a [`set_client_config`] hook is asked about.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ProviderTarget<'a> {
    /// Which table the row is from; [`build_provider`] reports [`LinkSource::Admin`].
    pub source: LinkSource,
    /// The row's provider key (`"google"`, `"oidc"`, …).
    pub provider: &'a str,
    pub issuer_url: Option<&'a str>,
}

type ClientConfigHook = Arc<dyn Fn(&ProviderTarget<'_>) -> Option<ClientConfig> + Send + Sync>;

static CLIENT_CONFIG: RwLock<Option<ClientConfigHook>> = RwLock::new(None);

/// Set the process-wide HTTP client hook for providers built from rows,
/// e.g. a private root CA per issuer. `None` from it keeps the default
/// client. Runs for discovery and every later call, as
/// [`OAuth2Provider::with_client_config`].
pub fn set_client_config(
    hook: impl Fn(&ProviderTarget<'_>) -> Option<ClientConfig> + Send + Sync + 'static,
) {
    *CLIENT_CONFIG
        .write()
        .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
}

fn client_config_for(target: &ProviderTarget<'_>) -> Option<ClientConfig> {
    let hook = CLIENT_CONFIG
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()?;
    hook(target)
}

/// Build an [`OAuth2Provider`] from a resolved config. Known keys use the
/// built-in presets; `"oidc"` runs OpenID Connect discovery against
/// `issuer_url`, reusing the result for an hour.
///
/// # Errors
/// [`SsoError::UnknownProvider`] for an unrecognized key,
/// [`SsoError::MissingIssuer`] for `oidc` without an issuer, or a
/// wrapped [`OAuthError`] if discovery fails.
pub async fn build_provider(cfg: &ResolvedSso) -> Result<OAuth2Provider, SsoError> {
    build_provider_with(cfg, LinkSource::Admin, &TargetPolicy::from_env()).await
}

/// [`build_provider`] for a row from `source`. A tenant admin owns its
/// rows, so they get public addresses only (plus the tenant allowlist),
/// never the operator's allowlist.
#[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
pub(crate) async fn build_provider_for(
    cfg: &ResolvedSso,
    source: LinkSource,
) -> Result<OAuth2Provider, SsoError> {
    let policy = match source {
        LinkSource::Tenant => TargetPolicy::tenant_from_env(),
        LinkSource::Shared | LinkSource::Admin | LinkSource::App => TargetPolicy::from_env(),
    };
    build_provider_with(cfg, source, &policy).await
}

async fn build_provider_with(
    cfg: &ResolvedSso,
    source: LinkSource,
    policy: &TargetPolicy,
) -> Result<OAuth2Provider, SsoError> {
    if cfg.client_id.trim().is_empty() {
        return Err(SsoError::Config("client_id is empty".into()));
    }
    let client = client_config_for(&ProviderTarget {
        source,
        provider: &cfg.provider,
        issuer_url: cfg.issuer_url.as_deref(),
    });
    let (id, secret, redirect) = (
        cfg.client_id.clone(),
        cfg.client_secret.clone(),
        cfg.redirect_uri.clone(),
    );
    let provider = match cfg.provider.as_str() {
        "google" => providers::google(id, secret, redirect),
        "microsoft" => providers::microsoft(id, secret, redirect),
        "github" => providers::github(id, secret, redirect),
        "gitlab" => providers::gitlab(id, secret, redirect),
        "discord" => providers::discord(id, secret, redirect),
        "oidc" => {
            let issuer = cfg.issuer_url.as_deref().ok_or(SsoError::MissingIssuer)?;
            discovered(issuer, id, secret, redirect, policy, client.clone()).await?
        }
        other => return Err(SsoError::UnknownProvider(other.to_owned())),
    };
    // Per-provider scope override (e.g. adding `groups` for an OIDC IdP).
    let provider = match &cfg.scopes {
        Some(s) if !s.is_empty() => provider.with_scopes(s.iter().cloned()),
        _ => provider,
    };
    let provider = match client {
        Some(c) => provider.with_client_config(move |b| c(b)),
        None => provider,
    };
    Ok(provider.with_egress(policy.clone()))
}

/// How long discovered endpoints are reused before the next login refetches them.
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// How long a failed discovery is reused, so logins do not each wait on a down IdP.
const DISCOVERY_FAIL_TTL: Duration = Duration::from_secs(30);
/// Most issuers cached at once; past it, the oldest is dropped.
const DISCOVERY_MAX: usize = 1024;

/// The public endpoints of one issuer. Keyed by issuer URL and holding no
/// credentials, so sharing them across tenants leaks nothing.
#[derive(Clone)]
struct Endpoints {
    auth_url: String,
    token_url: String,
    userinfo_url: Option<String>,
    id_token_keys: Option<(String, crate::oauth2::IssuerRule)>,
}

/// A discovery outcome: the endpoints, or the failure's message.
type Discovered = Result<Endpoints, String>;

/// Issuer URL, the outbound policy it was fetched under and whether a client
/// hook ran: an outcome under one says nothing about another.
type DiscoveryKey = (String, TargetPolicy, bool);

type Gate = std::sync::Arc<tokio::sync::Mutex<()>>;

static DISCOVERED: LazyLock<Mutex<HashMap<DiscoveryKey, (Instant, Discovered)>>> =
    LazyLock::new(Mutex::default);

/// One fetch per key at a time; the others wait for its result.
static IN_FLIGHT: LazyLock<Mutex<HashMap<DiscoveryKey, Gate>>> = LazyLock::new(Mutex::default);

/// Held by the caller that fetches: drops its gate when done or cancelled.
struct InFlight<'a>(&'a DiscoveryKey, &'a Gate);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut map = IN_FLIGHT.lock().unwrap_or_else(PoisonError::into_inner);
        if map
            .get(self.0)
            .is_some_and(|g| std::sync::Arc::ptr_eq(g, self.1))
        {
            map.remove(self.0);
        }
    }
}

fn ttl_of(found: &Discovered) -> Duration {
    if found.is_ok() {
        DISCOVERY_TTL
    } else {
        DISCOVERY_FAIL_TTL
    }
}

fn cached(key: &DiscoveryKey) -> Option<Discovered> {
    let map = DISCOVERED.lock().unwrap_or_else(PoisonError::into_inner);
    map.get(key)
        .filter(|(at, found)| at.elapsed() < ttl_of(found))
        .map(|(_, found)| found.clone())
}

/// An OIDC provider for `issuer`, running discovery at most once per [`DISCOVERY_TTL`]
/// (a failure, once per [`DISCOVERY_FAIL_TTL`]).
async fn discovered(
    issuer: &str,
    id: String,
    secret: String,
    redirect: String,
    policy: &TargetPolicy,
    client: Option<ClientConfig>,
) -> Result<OAuth2Provider, OAuthError> {
    let key: DiscoveryKey = (
        issuer.trim_end_matches('/').to_owned(),
        policy.clone(),
        client.is_some(),
    );
    let found = match cached(&key) {
        Some(found) => found,
        None => {
            let gate = IN_FLIGHT
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.clone())
                .or_default()
                .clone();
            let _turn = gate.lock().await;
            match cached(&key) {
                Some(found) => found,
                None => {
                    let _in_flight = InFlight(&key, &gate);
                    let mut probe = OAuth2Provider::new("oidc", "", "", "", "", "");
                    if let Some(c) = client {
                        probe = probe.with_client_config(move |b| c(b));
                    }
                    let fetched = probe.discover(&key.0, policy).await;
                    let found = fetched
                        .as_ref()
                        .map_err(ToString::to_string)
                        .map(|p| Endpoints {
                            auth_url: p.auth_url.clone(),
                            token_url: p.token_url.clone(),
                            userinfo_url: p.userinfo_url.clone(),
                            id_token_keys: p
                                .id_token_keys()
                                .map(|(u, i)| (u.to_owned(), i.clone())),
                        });
                    remember(
                        &mut DISCOVERED.lock().unwrap_or_else(PoisonError::into_inner),
                        key.clone(),
                        found.clone(),
                        DISCOVERY_MAX,
                    );
                    // The fetcher keeps the original error; waiters get its message.
                    fetched?;
                    found
                }
            }
        }
    };
    let e = found.map_err(OAuthError::Discovery)?;
    let mut p = OAuth2Provider::new("oidc", id, secret, redirect, e.auth_url, e.token_url);
    if let Some(u) = e.userinfo_url {
        p = p.with_userinfo_url(u);
    }
    if let Some((jwks, issuer)) = e.id_token_keys {
        p = p.with_id_token_keys(jwks, issuer);
    }
    Ok(p)
}

/// Insert `found`, dropping expired entries and then the oldest when `max` is reached.
fn remember<K: std::hash::Hash + Eq + Clone>(
    map: &mut HashMap<K, (Instant, Discovered)>,
    key: K,
    found: Discovered,
    max: usize,
) {
    map.retain(|_, (at, found)| at.elapsed() < ttl_of(found));
    if map.len() >= max && !map.contains_key(&key) {
        let oldest = map
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            map.remove(&k);
        }
    }
    map.insert(key, (Instant::now(), found));
}

/// One SSO button for a login page — the template loops over these.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderButton {
    /// Route key (`{login}/sso/{slug}`).
    pub slug: String,
    /// Button text (e.g. "Sign in with Google").
    pub label: String,
    /// Absolute-or-relative href the button links to.
    pub login_url: String,
}

/// Parse a stored space-separated scope string into the `ResolvedSso`
/// override form. Empty/blank → `None` (keep provider defaults).
#[must_use]
pub fn parse_scopes(scopes: Option<&str>) -> Option<Vec<String>> {
    let s = scopes?.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.split_whitespace().map(str::to_owned).collect())
}

/// Resolve a `secret_ref` for the **bare admin** (no tenancy in the
/// dependency set): `env://VAR` reads the environment; anything else is a
/// literal. The tenant surfaces use the richer
/// [`ChainSecretsResolver`](crate::tenancy::ChainSecretsResolver) instead.
///
/// # Errors
/// [`SsoError::Secret`] when an `env://` variable is unset.
pub fn resolve_secret_ref_env(reference: &str) -> Result<String, SsoError> {
    if let Some(var) = reference.strip_prefix("env://") {
        std::env::var(var).map_err(|_| SsoError::Secret(format!("env var `{var}` is unset")))
    } else {
        Ok(reference.to_owned())
    }
}

/// The verified email from a completed handshake, or an error when the
/// IdP didn't return a verified email address. Only opt-in email
/// linking ([`link::sign_in`]) uses it.
///
/// # Errors
/// [`SsoError::EmailNotVerified`] when `email_verified` is false or no
/// email was returned.
pub fn verified_email(user: &NormalizedUser) -> Result<&str, SsoError> {
    match (&user.email, user.email_verified) {
        (Some(e), true) if !e.is_empty() => Ok(e.as_str()),
        _ => Err(SsoError::EmailNotVerified),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn cfg(provider: &str) -> ResolvedSso {
        ResolvedSso {
            provider: provider.into(),
            issuer_url: None,
            client_id: "cid".into(),
            client_secret: "csecret".into(),
            redirect_uri: "https://app.example.com/login/sso/x/callback".into(),
            scopes: None,
        }
    }

    #[cfg(feature = "admin")]
    /// `__Host-` only for a host-only HTTPS cookie at `/`; the old name still reads.
    #[test]
    fn flow_cookies_take_the_host_prefix_only_at_the_root() {
        let root = FlowCookie::new("f", "/", true);
        assert_eq!(root.name(), "__Host-f");
        assert!(root.set("v", 600).starts_with("__Host-f=v; Path=/;"));
        assert!(root.set("v", 600).ends_with("; Secure"));
        assert_eq!(root.clear().len(), 2);
        assert_eq!(FlowCookie::new("f", "/acme", true).name(), "f");
        assert_eq!(FlowCookie::new("f", "/", false).name(), "f");
        // A plain-name cookie (tossed from a sibling subdomain) is ignored.
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::COOKIE, "f=planted".parse().unwrap());
        assert_eq!(root.read(&h), None);
        assert_eq!(
            FlowCookie::new("f", "/acme", true).read(&h),
            Some("planted")
        );
        h.insert(
            axum::http::header::COOKIE,
            "f=old; __Host-f=new".parse().unwrap(),
        );
        assert_eq!(root.read(&h), Some("new"));
    }

    #[cfg(feature = "admin")]
    /// A flow completes once.
    #[test]
    fn a_flow_is_claimed_once() {
        let (_, flow) = crate::oauth2::OAuth2Provider::new("t", "c", "s", "r", "a", "t").begin();
        assert!(claim_flow(&flow));
        assert!(!claim_flow(&flow));
        let (_, other) = crate::oauth2::OAuth2Provider::new("t", "c", "s", "r", "a", "t").begin();
        assert!(claim_flow(&other));
    }

    #[test]
    fn debug_hides_the_client_secret() {
        let s = format!("{:?}", cfg("google"));
        assert!(s.contains("cid") && !s.contains("csecret"), "{s}");
    }

    #[tokio::test]
    async fn presets_build_without_network() {
        for name in ["google", "microsoft", "github", "gitlab", "discord"] {
            let p = build_provider(&cfg(name)).await.expect("preset builds");
            assert_eq!(p.client_id, "cid");
            assert_eq!(
                p.redirect_uri,
                "https://app.example.com/login/sso/x/callback"
            );
        }
    }

    #[tokio::test]
    async fn scopes_override_is_applied() {
        // Default scopes stay when no override.
        let p = build_provider(&cfg("google")).await.expect("builds");
        assert_eq!(p.scopes, vec!["openid", "email", "profile"]);
        // An override replaces them (e.g. adding `groups` for an IdP).
        let mut c = cfg("google");
        c.scopes = Some(vec!["openid".into(), "email".into(), "groups".into()]);
        let p = build_provider(&c).await.expect("builds");
        assert_eq!(p.scopes, vec!["openid", "email", "groups"]);
    }

    #[test]
    fn parse_scopes_splits_and_trims() {
        assert_eq!(parse_scopes(None), None);
        assert_eq!(parse_scopes(Some("  ")), None);
        assert_eq!(
            parse_scopes(Some("openid  email profile")),
            Some(vec!["openid".into(), "email".into(), "profile".into()])
        );
    }

    #[test]
    fn resolve_secret_ref_env_reads_env_or_literal() {
        // A bare value is a literal.
        assert_eq!(
            resolve_secret_ref_env("plain-literal").unwrap(),
            "plain-literal"
        );
        // `env://VAR` reads the environment — `PATH` is reliably set.
        assert!(resolve_secret_ref_env("env://PATH").is_ok());
        // An unset var errors.
        assert!(resolve_secret_ref_env("env://RUSTANGO_TEST_UNSET_VAR_QQQ").is_err());
    }

    /// A discovery server that counts hits and fails the first `fail_first`.
    async fn issuer(fail_first: usize) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let iss = base.clone();
        let app = axum::Router::new().route(
            "/.well-known/openid-configuration",
            axum::routing::get(move || async move {
                if h.fetch_add(1, Ordering::SeqCst) < fail_first {
                    return Err(axum::http::StatusCode::BAD_GATEWAY);
                }
                Ok(axum::Json(serde_json::json!({
                    "issuer": iss,
                    "authorization_endpoint": "https://idp.test/auth",
                    "token_endpoint": "https://idp.test/token",
                })))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, hits)
    }

    fn oidc(issuer: &str, client_id: &str) -> ResolvedSso {
        let mut c = cfg("oidc");
        c.issuer_url = Some(issuer.to_owned());
        c.client_id = client_id.into();
        c
    }

    fn loopback() -> TargetPolicy {
        TargetPolicy::Public(crate::outbound::Allowlist::parse("127.0.0.1"))
    }

    /// #1833 — discovery runs once per issuer, not once per login.
    #[tokio::test]
    async fn discovery_is_cached_per_issuer() {
        let (base, hits) = issuer(0).await;
        let a = build_provider_with(&oidc(&base, "a"), LinkSource::Admin, &loopback())
            .await
            .unwrap();
        let b = build_provider_with(
            &oidc(&format!("{base}/"), "b"),
            LinkSource::Admin,
            &loopback(),
        )
        .await
        .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!((a.client_id.as_str(), b.client_id.as_str()), ("a", "b"));
        assert_eq!(b.token_url, "https://idp.test/token");
        assert_eq!(b.userinfo_url, None);
    }

    /// At the cap the oldest issuer is dropped, so a new one is still cached.
    #[test]
    fn a_full_cache_drops_its_oldest_issuer() {
        let e = || Endpoints {
            auth_url: "a".into(),
            token_url: "t".into(),
            userinfo_url: None,
            id_token_keys: None,
        };
        let mut map = HashMap::new();
        for k in ["one", "two", "three"] {
            remember(&mut map, k.into(), Ok(e()), 2);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut keys: Vec<_> = map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["three", "two"]);
    }

    /// A failed discovery is reused briefly, then retried.
    #[tokio::test]
    async fn failed_discovery_is_cached_briefly() {
        let (base, hits) = issuer(1).await;
        for _ in 0..3 {
            assert!(
                build_provider_with(&oidc(&base, "a"), LinkSource::Admin, &loopback())
                    .await
                    .is_err()
            );
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a failure is cached");
        // Age the failure past its TTL.
        let aged = Instant::now().checked_sub(DISCOVERY_FAIL_TTL).unwrap();
        DISCOVERED
            .lock()
            .unwrap()
            .get_mut(&(base.clone(), loopback(), false))
            .unwrap()
            .0 = aged;
        assert!(
            build_provider_with(&oidc(&base, "a"), LinkSource::Admin, &loopback())
                .await
                .is_ok()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// A failure under one policy is not replayed under another.
    #[tokio::test]
    async fn the_policy_is_part_of_the_cache_key() {
        let (base, hits) = issuer(1).await;
        let strict = TargetPolicy::Public(crate::outbound::Allowlist::default());
        assert!(
            build_provider_with(&oidc(&base, "a"), LinkSource::Admin, &strict)
                .await
                .is_err()
        );
        assert!(
            build_provider_with(&oidc(&base, "a"), LinkSource::Admin, &loopback())
                .await
                .is_err()
        );
        assert!(
            !IN_FLIGHT
                .lock()
                .unwrap()
                .contains_key(&(base.clone(), loopback(), false)),
            "a finished fetch leaves no gate"
        );
        assert!(
            build_provider_with(&oidc(&base, "a"), LinkSource::Admin, &strict)
                .await
                .is_err()
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "strict never reached the server"
        );
    }

    /// Concurrent logins against a cold issuer share one fetch.
    #[tokio::test]
    async fn concurrent_discovery_fetches_once() {
        let (base, hits) = issuer(0).await;
        let cfg = oidc(&base, "a");
        let policy = loopback();
        let runs = (0..8).map(|_| build_provider_with(&cfg, LinkSource::Admin, &policy));
        for r in futures_util::future::join_all(runs).await {
            r.unwrap();
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// A tenant-owned row never reaches the operator's allowlisted hosts;
    /// a shared one does. Discovery and the token exchange both.
    #[tokio::test]
    async fn tenant_rows_ignore_the_operator_allowlist() {
        let _g = crate::outbound::ENV_LOCK.lock().await;
        let (base, hits) = issuer(0).await;
        std::env::set_var(crate::outbound::ALLOW_ENV, "127.0.0.1");
        let tenant = build_provider_for(&oidc(&base, "t"), LinkSource::Tenant).await;
        let shared = build_provider_for(&oidc(&base, "s"), LinkSource::Shared).await;
        let mut preset = build_provider_for(&cfg("google"), LinkSource::Tenant)
            .await
            .unwrap();
        preset.token_url = format!("{base}/token");
        let (_, flow) = preset.begin();
        let state = flow.state.clone();
        let done = preset.complete(&flow, "code", &state).await;
        std::env::remove_var(crate::outbound::ALLOW_ENV);
        // Only the tenant allowlist opens a private host to a tenant row.
        std::env::set_var(crate::outbound::TENANT_ALLOW_ENV, "127.0.0.1");
        let opted = build_provider_for(&oidc(&base, "o"), LinkSource::Tenant).await;
        std::env::remove_var(crate::outbound::TENANT_ALLOW_ENV);
        assert!(tenant.is_err(), "tenant row reached an allowlisted host");
        assert!(shared.is_ok(), "{:?}", shared.err());
        assert!(opted.is_ok(), "{:?}", opted.err());
        // The same policy as the shared row's, so its discovery is reused.
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let err = done.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("blocked"), "token POST not refused: {err}");
    }

    /// A row's provider gets the hook's client: discovery runs through it,
    /// and the provider keeps it for the token calls.
    #[tokio::test]
    async fn the_client_hook_reaches_discovery_and_the_provider() {
        let (base, _) = issuer(0).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (c, s, iss) = (calls.clone(), seen.clone(), base.clone());
        set_client_config(move |t| {
            if t.issuer_url != Some(iss.as_str()) {
                return None;
            }
            s.lock().unwrap().push(t.source);
            let c = c.clone();
            Some(Arc::new(move |b: reqwest::ClientBuilder| {
                c.fetch_add(1, Ordering::SeqCst);
                b
            }) as ClientConfig)
        });
        let p = build_provider_with(&oidc(&base, "t"), LinkSource::Tenant, &loopback())
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "discovery skipped the hook"
        );
        assert!(format!("{p:?}").contains("client_config: true"), "{p:?}");
        assert_eq!(*seen.lock().unwrap(), [LinkSource::Tenant]);
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected() {
        let e = build_provider(&cfg("myspace")).await.unwrap_err();
        assert!(matches!(e, SsoError::UnknownProvider(p) if p == "myspace"));
    }

    #[tokio::test]
    async fn oidc_without_issuer_is_rejected() {
        assert!(matches!(
            build_provider(&cfg("oidc")).await.unwrap_err(),
            SsoError::MissingIssuer
        ));
    }

    #[tokio::test]
    async fn empty_client_id_is_rejected() {
        let mut c = cfg("google");
        c.client_id = "  ".into();
        assert!(matches!(
            build_provider(&c).await.unwrap_err(),
            SsoError::Config(_)
        ));
    }

    #[test]
    fn verified_email_requires_verified_flag() {
        let mut u = NormalizedUser {
            provider: "google".into(),
            provider_user_id: "1".into(),
            email: Some("a@example.com".into()),
            email_verified: false,
            name: None,
            avatar_url: None,
            raw: serde_json::json!({}),
        };
        assert!(verified_email(&u).is_err());
        u.email_verified = true;
        assert_eq!(verified_email(&u).unwrap(), "a@example.com");
    }
}
