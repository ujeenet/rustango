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
//! ([`OAuth2Provider::begin`]/[`complete`](OAuth2Provider::complete) +
//! [`seal_flow`]/[`open_flow`]) to prove identity, then the *caller*
//! links the verified email to an existing user and mints that surface's
//! normal session cookie. The admin flow is **link-to-existing** — SSO
//! never auto-provisions an admin; an unknown or unverified email is
//! refused. (The member flow may opt into auto-provisioning.)
//!
//! The client secret is resolved from a reference (`env://…`) by the
//! caller before building the provider, so the raw secret never lands in
//! a DB column or a config file (mirrors `Org.database_url`).

pub mod provider;
pub use provider::{list_enabled, resolve_by_slug, SsoProvider};

use crate::oauth2::{providers, OAuth2Provider, OAuthError};

pub use crate::oauth2::{open_flow, seal_flow, NormalizedUser, OAuth2Flow};

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
/// `issuer_url`.
///
/// # Errors
/// [`SsoError::UnknownProvider`] for an unrecognized key,
/// [`SsoError::MissingIssuer`] for `oidc` without an issuer, or a
/// wrapped [`OAuthError`] if discovery fails.
pub async fn build_provider(cfg: &ResolvedSso) -> Result<OAuth2Provider, SsoError> {
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
            OAuth2Provider::from_discovery("oidc", issuer, id, secret, redirect).await?
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
/// [`crate::tenancy::secrets::ChainSecretsResolver`] instead.
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
/// IdP didn't return a verified email address. Both surfaces link on
/// this value.
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
