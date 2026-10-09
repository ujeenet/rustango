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
//! The client secret is resolved from a reference (`env://…`) by the
//! caller before building the provider, so the raw secret never lands in
//! a DB column or a config file (mirrors `Org.database_url`).
//!
//! [`SsoProvider`]: crate::sso::SsoProvider
//! [`OAuth2Provider`]: crate::oauth2::OAuth2Provider
//! [`OAuth2Provider::begin`]: crate::oauth2::OAuth2Provider::begin
//! [`seal_flow`]: crate::oauth2::seal_flow
//! [`open_flow`]: crate::oauth2::open_flow

#[cfg(any(feature = "tenancy", feature = "admin-sso"))]
#[doc(hidden)]
pub mod check;
pub mod link;
pub mod provider;
pub use link::{LinkSource, ProviderKey, SsoLink};
pub use provider::{list_enabled, resolve_by_slug, ResolvedProvider, SsoProvider};

use crate::oauth2::{providers, OAuth2Provider, OAuthError};
use crate::outbound::TargetPolicy;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub use crate::oauth2::{open_flow, seal_flow, FlowPurpose, FlowScope, NormalizedUser, OAuth2Flow};

/// Cookie the sealed [`OAuth2Flow`] round-trips in between the login
/// redirect and the callback. Distinct from the standalone
/// `oauth2::router`'s `rustango_oauth_flow` so the two can coexist.
pub const SSO_FLOW_COOKIE: &str = "rustango_admin_sso_flow";

/// A fully-resolved SSO provider config — the client secret is the
/// dereferenced value (not an `env://` reference), ready to build a
/// provider. Both surfaces normalize their stored config into this.
#[derive(Debug, Clone)]
pub struct ResolvedSso {
    /// Provider key: `"google"`, `"microsoft"`, `"github"`, `"gitlab"`,
    /// `"discord"`, or `"oidc"` for a generic OpenID Connect provider.
    pub provider: String,
    /// OIDC issuer base URL — required when `provider == "oidc"`.
    pub issuer_url: Option<String>,
    pub client_id: String,
    /// Resolved secret value (already dereferenced from `env://…`).
    pub client_secret: String,
    /// Must match the route mounted at `<login>/sso/{provider}/callback`.
    pub redirect_uri: String,
    /// Optional OAuth scope override. `None` keeps the provider defaults
    /// (`openid email profile`); `Some(list)` replaces them.
    pub scopes: Option<Vec<String>>,
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

/// Build an [`OAuth2Provider`] from a resolved config. Known keys use the
/// built-in presets; `"oidc"` runs OpenID Connect discovery against
/// `issuer_url`, reusing the result for an hour.
///
/// # Errors
/// [`SsoError::UnknownProvider`] for an unrecognized key,
/// [`SsoError::MissingIssuer`] for `oidc` without an issuer, or a
/// wrapped [`OAuthError`] if discovery fails.
pub async fn build_provider(cfg: &ResolvedSso) -> Result<OAuth2Provider, SsoError> {
    build_provider_with(cfg, &TargetPolicy::from_env()).await
}

async fn build_provider_with(
    cfg: &ResolvedSso,
    policy: &TargetPolicy,
) -> Result<OAuth2Provider, SsoError> {
    if cfg.client_id.trim().is_empty() {
        return Err(SsoError::Config("client_id is empty".into()));
    }
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
            discovered(issuer, id, secret, redirect, policy).await?
        }
        other => return Err(SsoError::UnknownProvider(other.to_owned())),
    };
    // Per-provider scope override (e.g. adding `groups` for an OIDC IdP).
    let provider = match &cfg.scopes {
        Some(s) if !s.is_empty() => provider.with_scopes(s.iter().cloned()),
        _ => provider,
    };
    Ok(provider)
}

/// How long discovered endpoints are reused before the next login refetches them.
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// Most issuers cached at once; past it, the oldest is dropped.
const DISCOVERY_MAX: usize = 1024;

/// The public endpoints of one issuer. Keyed by issuer URL and holding no
/// credentials, so sharing them across tenants leaks nothing.
#[derive(Clone)]
struct Endpoints {
    auth_url: String,
    token_url: String,
    userinfo_url: Option<String>,
}

static DISCOVERED: LazyLock<Mutex<HashMap<String, (Instant, Endpoints)>>> =
    LazyLock::new(Mutex::default);

/// An OIDC provider for `issuer`, running discovery at most once per [`DISCOVERY_TTL`].
async fn discovered(
    issuer: &str,
    id: String,
    secret: String,
    redirect: String,
    policy: &TargetPolicy,
) -> Result<OAuth2Provider, OAuthError> {
    let key = issuer.trim_end_matches('/').to_owned();
    let hit = {
        let map = DISCOVERED.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(&key)
            .filter(|(at, _)| at.elapsed() < DISCOVERY_TTL)
            .map(|(_, e)| e.clone())
    };
    if let Some(e) = hit {
        let p = OAuth2Provider::new("oidc", id, secret, redirect, e.auth_url, e.token_url);
        return Ok(match e.userinfo_url {
            Some(u) => p.with_userinfo_url(u),
            None => p,
        });
    }
    let p = OAuth2Provider::new("oidc", id, secret, redirect, "", "")
        .discover(&key, policy)
        .await?;
    let e = Endpoints {
        auth_url: p.auth_url.clone(),
        token_url: p.token_url.clone(),
        userinfo_url: p.userinfo_url.clone(),
    };
    let mut map = DISCOVERED.lock().unwrap_or_else(PoisonError::into_inner);
    remember(&mut map, key, e, DISCOVERY_MAX);
    Ok(p)
}

/// Insert `e`, dropping expired entries and then the oldest when `max` is reached.
fn remember(
    map: &mut HashMap<String, (Instant, Endpoints)>,
    key: String,
    e: Endpoints,
    max: usize,
) {
    map.retain(|_, (at, _)| at.elapsed() < DISCOVERY_TTL);
    if map.len() >= max && !map.contains_key(&key) {
        let oldest = map
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            map.remove(&k);
        }
    }
    map.insert(key, (Instant::now(), e));
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
        let app = axum::Router::new().route(
            "/.well-known/openid-configuration",
            axum::routing::get(move || async move {
                if h.fetch_add(1, Ordering::SeqCst) < fail_first {
                    return Err(axum::http::StatusCode::BAD_GATEWAY);
                }
                Ok(axum::Json(serde_json::json!({
                    "authorization_endpoint": "https://idp.test/auth",
                    "token_endpoint": "https://idp.test/token",
                })))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
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
        let a = build_provider_with(&oidc(&base, "a"), &loopback())
            .await
            .unwrap();
        let b = build_provider_with(&oidc(&format!("{base}/"), "b"), &loopback())
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
        };
        let mut map = HashMap::new();
        for k in ["one", "two", "three"] {
            remember(&mut map, k.into(), e(), 2);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut keys: Vec<_> = map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["three", "two"]);
    }

    /// A failed discovery is not cached; the next login retries.
    #[tokio::test]
    async fn failed_discovery_is_retried() {
        let (base, hits) = issuer(1).await;
        assert!(build_provider_with(&oidc(&base, "a"), &loopback())
            .await
            .is_err());
        assert!(build_provider_with(&oidc(&base, "a"), &loopback())
            .await
            .is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
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
