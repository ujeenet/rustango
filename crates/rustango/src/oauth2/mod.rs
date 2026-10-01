//! OAuth2 / OIDC swiss-knife — one type, every provider.
//!
//! [`OAuth2Provider`] handles **both** pure OAuth2 (GitHub, Discord, Slack)
//! and OpenID Connect (Google, Microsoft, Apple, Keycloak, Auth0, Okta) by
//! treating the provider's `/userinfo` endpoint as the identity source. We
//! skip `id_token` JWT verification on purpose — TLS to a discovered
//! `userinfo_endpoint` is the trust anchor instead. That trade buys
//! uniform handling across both protocol shapes and avoids pulling a
//! JWT/JWKS stack into the dep tree.
//!
//! ## Security notes (audit L2)
//!
//! * **CSRF is covered by the `state` parameter** (CSPRNG-generated,
//!   sealed into the flow cookie, constant-time compared on callback).
//! * **No OIDC `nonce`** is sent. With the userinfo-over-TLS model the
//!   `id_token` is never trusted, so a `nonce` (which binds a session to
//!   an `id_token`) has nothing to bind to; `state` carries the CSRF
//!   guarantee. If you switch to trusting the `id_token`, add + verify a
//!   `nonce`.
//! * **`redirect_uri` is not re-validated locally** — it's fixed per
//!   provider at registration and enforced by the provider's allowlist.
//!   Keep the registered `redirect_uri` exact (no open wildcards) on the
//!   provider side.
//!
//! ## Quick start (built-in provider)
//!
//! ```ignore
//! use rustango::oauth2::{providers, OAuth2Provider};
//!
//! let google = providers::google(
//!     std::env::var("GOOGLE_CLIENT_ID").unwrap(),
//!     std::env::var("GOOGLE_CLIENT_SECRET").unwrap(),
//!     "https://app.example.com/auth/google/callback".to_owned(),
//! );
//!
//! // 1. Login route — redirect the browser to `auth_url` and stash `flow`
//! //    in the session (cookie / cache / DB) keyed by `flow.state`.
//! let (auth_url, flow) = google.begin();
//!
//! // 2. Callback route — exchange the code, fetch userinfo:
//! let (user, tokens) = google.complete(&flow, code, state).await?;
//! //    user.email is the canonical identity; persist it however you like.
//! ```
//!
//! ## Per-tenant configuration
//!
//! [`OAuth2Registry`] maps `(tenant_id, provider_name)` to a configured
//! [`OAuth2Provider`]. Build it from config at startup, OR back it with the
//! DB so tenants can rotate keys from the admin without redeploys.
//!
//! ## OIDC discovery
//!
//! [`OAuth2Provider::from_discovery`] fetches `.well-known/openid-configuration`
//! and populates `auth_url`, `token_url`, `userinfo_url` automatically. Use it
//! for any conformant OIDC provider — Keycloak, Auth0, Okta, etc.
//!
//! [`OAuth2Registry`]: crate::oauth2::OAuth2Registry
//! [`OAuth2Provider`]: crate::oauth2::OAuth2Provider
//! [`OAuth2Provider::from_discovery`]: crate::oauth2::OAuth2Provider::from_discovery

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::outbound::{self, bounded_text, capped_body, BodyError, EgressCache, TargetPolicy};

pub mod providers;
pub mod registry;
// The router only needs axum, which arrives via `manage` (and `admin`
// implies `manage`, so existing admin builds keep it). Gating on `admin`
// was a stale proxy for "axum present" that made `oauth2_router`
// unreachable for non-admin apps that enable just `oauth2` + `manage`.
#[cfg(feature = "manage")]
pub mod router;

pub use registry::OAuth2Registry;

// --------------------------------------------------------------------- errors

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("HTTP transport error: {0}")]
    Http(String),
    #[error("provider returned non-success status {status}: {body}")]
    BadStatus { status: u16, body: String },
    #[error("response body did not deserialize: {0}")]
    BadResponse(String),
    #[error("CSRF state mismatch")]
    StateMismatch,
    #[error("login flow expired — restart at /login")]
    FlowExpired,
    #[error("PKCE verifier missing — flow not initialized")]
    MissingPkce,
    #[error("missing required field `{0}` in userinfo response")]
    MissingField(&'static str),
    #[error("provider config invalid: {0}")]
    BadConfig(&'static str),
    #[error("OIDC discovery failed: {0}")]
    Discovery(String),
}

#[cfg(feature = "oauth2")]
impl From<reqwest::Error> for OAuthError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

// --------------------------------------------------------------------- types

/// Output of `/userinfo` (or equivalent) normalized across providers.
///
/// `provider_user_id` is the stable key — email can change, names get
/// edited, but the provider's user id is forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedUser {
    pub provider: String,
    pub provider_user_id: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    /// Raw provider response for fields we didn't normalize.
    pub raw: serde_json::Value,
}

/// Token bag returned by the provider's `/token` endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub token_type: Option<String>,
    /// Present on OIDC providers; we don't verify it (we use /userinfo).
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Maximum age of an OAuth2 login flow (audit N7). `complete()` rejects
/// a flow whose sealed `created_at` is older than this, bounding replay
/// of a captured sealed-flow value. 10 minutes gives headroom over the
/// 5-minute cookie `Max-Age` for slow logins while still being short.
const MAX_FLOW_AGE_SECS: u64 = 600;

/// Per-flow secret state — must be persisted between `begin()` and
/// `complete()`. Round-trip via signed cookie, server-side cache, or DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuth2Flow {
    pub state: String,
    pub pkce_verifier: String,
    pub created_at: u64,
}

/// User mapper — pluggable so providers with weird shapes (GitHub puts
/// email at a different endpoint, Discord uses `id` as a string, ...)
/// can normalize their userinfo into [`NormalizedUser`].
pub type UserMapper = Arc<
    dyn Fn(&str, serde_json::Value, &TokenResponse) -> Result<NormalizedUser, OAuthError>
        + Send
        + Sync,
>;

/// Changes the HTTP client builder, e.g. to add a root CA or an mTLS
/// identity. Redirect, proxy and address checks are set after it runs.
pub type ClientConfig = Arc<dyn Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync>;

/// Configuration for one OAuth2/OIDC provider.
///
/// Construct via [`OAuth2Provider::new`], a built-in preset
/// (e.g. [`providers::google`]), or [`OAuth2Provider::from_discovery`] for
/// any conformant OIDC issuer.
pub struct OAuth2Provider {
    pub name: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub auth_url: String,
    pub token_url: String,
    pub userinfo_url: Option<String>,
    pub scopes: Vec<String>,
    /// Extra parameters appended to the authorization URL (e.g. `prompt=consent`).
    pub extra_auth_params: Vec<(String, String)>,
    pub use_pkce: bool,
    /// Custom mapper. Defaults to [`default_user_mapper`] (works for OIDC
    /// providers where userinfo follows OIDC claims spec).
    pub user_mapper: UserMapper,
    /// Fetched after userinfo into `raw["emails"]` (GitHub `/user/emails`).
    emails_url: Option<String>,
    client_config: Option<ClientConfig>,
    http: EgressCache,
}

impl std::fmt::Debug for OAuth2Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2Provider")
            .field("name", &self.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("userinfo_url", &self.userinfo_url)
            .field("emails_url", &self.emails_url)
            .field("scopes", &self.scopes)
            .field("use_pkce", &self.use_pkce)
            .field("client_config", &self.client_config.is_some())
            .finish()
    }
}

impl OAuth2Provider {
    /// Bare-bones constructor. Most callers want a preset from
    /// [`providers`] or [`OAuth2Provider::from_discovery`].
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
        auth_url: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            redirect_uri: redirect_uri.into(),
            auth_url: auth_url.into(),
            token_url: token_url.into(),
            userinfo_url: None,
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
            extra_auth_params: Vec::new(),
            use_pkce: true,
            user_mapper: Arc::new(default_user_mapper),
            emails_url: None,
            client_config: None,
            http: EgressCache::default(),
        }
    }

    #[must_use]
    pub fn with_userinfo_url(mut self, url: impl Into<String>) -> Self {
        self.userinfo_url = Some(url.into());
        self
    }

    /// Also GET `url` with the access token and put the JSON in
    /// `raw["emails"]`, for a mapper that reads the verified flag there.
    #[must_use]
    pub fn with_emails_url(mut self, url: impl Into<String>) -> Self {
        self.emails_url = Some(url.into());
        self
    }

    #[must_use]
    pub fn with_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_extra_auth_params<I, K, V>(mut self, params: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.extra_auth_params = params
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    #[must_use]
    pub fn with_user_mapper(mut self, mapper: UserMapper) -> Self {
        self.user_mapper = mapper;
        self
    }

    #[must_use]
    pub fn with_pkce(mut self, on: bool) -> Self {
        self.use_pkce = on;
        self
    }

    /// Adjust the token and userinfo HTTP client, e.g. a private root CA.
    /// It runs once per provider and the client is reused; call this again
    /// to pick up a rotated cert. Any proxy it sets is dropped (use
    /// `RUSTANGO_OUTBOUND_PROXY`); a `.resolve` override skips the
    /// connect-time address check.
    #[must_use]
    pub fn with_client_config(
        mut self,
        f: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static,
    ) -> Self {
        self.client_config = Some(Arc::new(f));
        self.http = EgressCache::default();
        self
    }

    /// Load endpoints from `.well-known/openid-configuration` and return
    /// a configured provider. Pass the **issuer** URL — the discovery URL
    /// is appended for you (with or without a trailing slash).
    pub async fn from_discovery(
        name: impl Into<String>,
        issuer: impl AsRef<str>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Result<Self, OAuthError> {
        let p = Self::new(name, client_id, client_secret, redirect_uri, "", "");
        p.discover(issuer.as_ref(), &TargetPolicy::from_env()).await
    }

    /// [`Self::from_discovery`] with a [`ClientConfig`] used for discovery
    /// and every later call.
    pub async fn from_discovery_with(
        name: impl Into<String>,
        issuer: impl AsRef<str>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
        config: ClientConfig,
    ) -> Result<Self, OAuthError> {
        let mut p = Self::new(name, client_id, client_secret, redirect_uri, "", "");
        p.client_config = Some(config);
        p.discover(issuer.as_ref(), &TargetPolicy::from_env()).await
    }

    /// Fill the endpoints from `issuer`'s discovery document.
    pub(crate) async fn discover(
        mut self,
        issuer: &str,
        policy: &TargetPolicy,
    ) -> Result<Self, OAuthError> {
        let issuer = issuer.trim_end_matches('/');
        let url = format!("{issuer}/.well-known/openid-configuration");
        let resp = self
            .checked(&url, reqwest::Method::GET, policy)
            .await?
            .send()
            .await
            .map_err(|e| OAuthError::Discovery(format!("GET {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = bounded_text(resp, ERROR_BODY_MAX).await;
            return Err(OAuthError::Discovery(format!(
                "GET {url} -> {status}: {body}"
            )));
        }
        let bytes = capped_body(resp, SUCCESS_BODY_MAX)
            .await
            .map_err(|e| OAuthError::Discovery(format!("GET {url}: {e}")))?;
        let doc: DiscoveryDoc = serde_json::from_slice(&bytes)
            .map_err(|e| OAuthError::Discovery(format!("decode discovery doc: {e}")))?;
        self.auth_url = doc.authorization_endpoint;
        self.token_url = doc.token_endpoint;
        self.userinfo_url = doc.userinfo_endpoint;
        Ok(self)
    }

    /// Build the authorization URL the user's browser should be redirected to,
    /// plus the per-flow secret state to persist between this call and
    /// [`OAuth2Provider::complete`].
    #[must_use]
    pub fn begin(&self) -> (String, OAuth2Flow) {
        let state = random_token(32);
        let pkce_verifier = random_token(64);
        let pkce_challenge = pkce_s256_challenge(&pkce_verifier);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let mut params: Vec<(&str, String)> = vec![
            ("response_type", "code".into()),
            ("client_id", self.client_id.clone()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("scope", self.scopes.join(" ")),
            ("state", state.clone()),
        ];
        if self.use_pkce {
            params.push(("code_challenge", pkce_challenge));
            params.push(("code_challenge_method", "S256".into()));
        }
        let mut url = self.auth_url.clone();
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&encode_form(params.iter().map(|(k, v)| (*k, v.as_str()))));
        for (k, v) in &self.extra_auth_params {
            url.push('&');
            url.push_str(&urlencoding::encode(k));
            url.push('=');
            url.push_str(&urlencoding::encode(v));
        }

        let flow = OAuth2Flow {
            state,
            pkce_verifier,
            created_at: now,
        };
        (url, flow)
    }

    /// Exchange the auth code for tokens, then fetch `/userinfo` and
    /// return both. `flow` must be the value returned from the matching
    /// [`OAuth2Provider::begin`] call.
    pub async fn complete(
        &self,
        flow: &OAuth2Flow,
        code: &str,
        callback_state: &str,
    ) -> Result<(NormalizedUser, TokenResponse), OAuthError> {
        self.complete_with(flow, code, callback_state, &TargetPolicy::from_env())
            .await
    }

    async fn complete_with(
        &self,
        flow: &OAuth2Flow,
        code: &str,
        callback_state: &str,
        policy: &TargetPolicy,
    ) -> Result<(NormalizedUser, TokenResponse), OAuthError> {
        if flow
            .state
            .as_bytes()
            .ct_eq(callback_state.as_bytes())
            .unwrap_u8()
            == 0
        {
            return Err(OAuthError::StateMismatch);
        }

        // Audit N7 — enforce flow freshness server-side. `created_at` is
        // inside the HMAC-sealed flow, so it can't be forged; without this
        // a captured sealed flow would be replayable indefinitely (the
        // 5-minute cookie Max-Age is client-side and outside the MAC).
        // saturating_sub tolerates a created_at slightly in the future
        // (clock skew) by treating the age as 0.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if now.saturating_sub(flow.created_at) > MAX_FLOW_AGE_SECS {
            return Err(OAuthError::FlowExpired);
        }

        let mut body: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
        ];
        if self.use_pkce {
            body.push(("code_verifier", &flow.pkce_verifier));
        }
        let resp = self
            .checked(&self.token_url, reqwest::Method::POST, policy)
            .await?
            .header("Accept", "application/json")
            .form(&body)
            .send()
            .await?;
        let tokens = decode_or_error::<TokenResponse>(resp).await?;

        let userinfo_url = self
            .userinfo_url
            .as_deref()
            .ok_or(OAuthError::BadConfig("userinfo_url not set"))?;

        let resp = self
            .checked(userinfo_url, reqwest::Method::GET, policy)
            .await?
            .bearer_auth(&tokens.access_token)
            .header("Accept", "application/json")
            .send()
            .await?;
        let mut raw: serde_json::Value = decode_or_error(resp).await?;
        if let Some(url) = self.emails_url.as_deref() {
            let resp = self
                .checked(url, reqwest::Method::GET, policy)
                .await?
                .bearer_auth(&tokens.access_token)
                .header("Accept", "application/json")
                .send()
                .await?;
            // No `user:email` grant (403) or endpoint (404): no verified email, not a failure.
            let denied = matches!(
                resp.status(),
                reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
            );
            if !denied {
                let emails: serde_json::Value = decode_or_error(resp).await?;
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert("emails".to_owned(), emails);
                }
            }
        }

        let user = (self.user_mapper)(&self.name, raw, &tokens)?;
        Ok((user, tokens))
    }
}

// --------------------------------------------------------------------- helpers

/// Most bytes of a failed response body kept in an error.
const ERROR_BODY_MAX: usize = 256;
/// Cap on a success body: discovery, token and userinfo JSON (#1793).
const SUCCESS_BODY_MAX: usize = 1 << 20;
/// Timeout for discovery, token and userinfo calls.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

impl OAuth2Provider {
    /// Check `url`: a tenant can set the issuer, so discovered endpoints
    /// may point anywhere (#1716). Clients are reused across calls (#1792).
    async fn checked(
        &self,
        url: &str,
        method: reqwest::Method,
        policy: &TargetPolicy,
    ) -> Result<reqwest::RequestBuilder, OAuthError> {
        // The hook may set its own timeout, so only the shared client gets one per request.
        let (egress, timeout) = match &self.client_config {
            Some(config) => (
                self.http.get(policy.clone(), || {
                    config(reqwest::Client::builder().timeout(HTTP_TIMEOUT))
                })?,
                None,
            ),
            None => (outbound::shared(policy.clone())?, Some(HTTP_TIMEOUT)),
        };
        let target = egress
            .check(url)
            .await
            .map_err(|e| OAuthError::Http(e.to_string()))?;
        let req = target.request(method);
        Ok(match timeout {
            Some(t) => req.timeout(t),
            None => req,
        })
    }
}

#[derive(Deserialize)]
struct DiscoveryDoc {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

async fn decode_or_error<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, OAuthError> {
    let status = resp.status();
    if !status.is_success() {
        let body = bounded_text(resp, ERROR_BODY_MAX).await;
        return Err(OAuthError::BadStatus {
            status: status.as_u16(),
            body,
        });
    }
    let bytes = capped_body(resp, SUCCESS_BODY_MAX)
        .await
        .map_err(|e| match e {
            BodyError::Read(e) => OAuthError::from(e),
            too_large => OAuthError::BadResponse(too_large.to_string()),
        })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        let preview = String::from_utf8_lossy(&bytes)
            .chars()
            .take(200)
            .collect::<String>();
        OAuthError::BadResponse(format!("{e} (body: {preview})"))
    })
}

/// Default mapper — assumes the provider returns OIDC-style claims:
/// `sub`, `email`, `email_verified`, `name`, `picture`. Works as-is for
/// Google, Microsoft, Apple, Keycloak, Auth0, Okta, and most "OIDC-conformant"
/// providers. For pure OAuth2 (GitHub, Discord) use the per-provider mappers
/// in [`providers`].
pub fn default_user_mapper(
    provider: &str,
    raw: serde_json::Value,
    _tokens: &TokenResponse,
) -> Result<NormalizedUser, OAuthError> {
    let sub = raw
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or(OAuthError::MissingField("sub"))?
        .to_owned();
    let email = raw.get("email").and_then(|v| v.as_str()).map(str::to_owned);
    let email_verified = raw
        .get("email_verified")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let name = raw.get("name").and_then(|v| v.as_str()).map(str::to_owned);
    let avatar_url = raw
        .get("picture")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok(NormalizedUser {
        provider: provider.to_owned(),
        provider_user_id: sub,
        email,
        email_verified,
        name,
        avatar_url,
        raw,
    })
}

/// Random URL-safe token of `byte_len` bytes (output is ~`byte_len * 4 / 3`
/// chars). Used for OAuth2 `state` (CSRF) and PKCE verifiers — both
/// must be unpredictable to attackers, so we source from the OS CSPRNG.
/// v0.42.
fn random_token(byte_len: usize) -> String {
    use rand::{rngs::OsRng, RngCore};
    let mut buf = vec![0u8; byte_len];
    OsRng.fill_bytes(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(&buf)
}

/// PKCE S256 challenge — `BASE64URL(SHA256(verifier))`.
fn pkce_s256_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn encode_form<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut out = String::new();
    let mut first = true;
    for (k, v) in pairs {
        if !first {
            out.push('&');
        }
        first = false;
        out.push_str(&urlencoding::encode(k));
        out.push('=');
        out.push_str(&urlencoding::encode(v));
    }
    out
}

// --------------------------------------------------------------------- flow signing
//
// Stateless flow round-trip — sign the OAuth2Flow with HMAC so the callback
// can be validated without any server-side store. Useful when the request
// hits a different process than `begin()` did (multi-replica, no sticky
// sessions).

/// Sign `flow` with `secret`. Embed the result in a cookie or query param.
#[must_use]
pub fn seal_flow(flow: &OAuth2Flow, secret: &[u8]) -> String {
    let payload = serde_json::to_vec(flow).unwrap_or_default();
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC key");
    mac.update(payload_b64.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload_b64}.{sig}")
}

/// Verify and decode a flow sealed with [`seal_flow`].
pub fn open_flow(sealed: &str, secret: &[u8]) -> Result<OAuth2Flow, OAuthError> {
    let (payload_b64, sig_b64) = sealed
        .split_once('.')
        .ok_or(OAuthError::BadResponse("malformed sealed flow".into()))?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC key");
    mac.update(payload_b64.as_bytes());
    let expected = mac.finalize().into_bytes();
    let provided = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| OAuthError::BadResponse("bad signature encoding".into()))?;
    if expected.ct_eq(&provided).unwrap_u8() == 0 {
        return Err(OAuthError::StateMismatch);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| OAuthError::BadResponse("bad payload encoding".into()))?;
    serde_json::from_slice(&payload)
        .map_err(|e| OAuthError::BadResponse(format!("decode flow: {e}")))
}

// --------------------------------------------------------------------- key

/// Composite registry key — `(tenant_id, provider_name)`. Use the empty
/// string for tenant when running single-tenant.
pub type ProviderKey = (String, String);

/// Convenience: build the key from any pair of `Into<String>`.
pub fn provider_key(tenant: impl Into<String>, name: impl Into<String>) -> ProviderKey {
    (tenant.into(), name.into())
}

/// Helper that builds a `HashMap<String, OAuth2Provider>` from `(name, provider)`
/// tuples. Used by single-tenant apps that don't want a registry.
pub fn map_providers<I>(providers: I) -> HashMap<String, OAuth2Provider>
where
    I: IntoIterator<Item = (String, OAuth2Provider)>,
{
    providers.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636_appendix_b_test_vector() {
        // RFC 7636, Appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_s256_challenge(verifier), expected);
    }

    #[test]
    fn random_token_is_distinct_each_call() {
        let a = random_token(32);
        let b = random_token(32);
        assert_ne!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn random_token_is_url_safe() {
        let t = random_token(32);
        assert!(t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn begin_includes_state_and_pkce() {
        let p = OAuth2Provider::new(
            "test",
            "cid",
            "csec",
            "https://app.test/callback",
            "https://idp.test/auth",
            "https://idp.test/token",
        );
        let (url, flow) = p.begin();
        assert!(url.starts_with("https://idp.test/auth?"));
        assert!(url.contains(&format!("state={}", flow.state)));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("scope=openid{}email{}profile", "%20", "%20")));
    }

    #[test]
    fn begin_without_pkce_omits_challenge() {
        let p = OAuth2Provider::new(
            "test",
            "cid",
            "csec",
            "https://app.test/cb",
            "https://idp.test/auth",
            "https://idp.test/token",
        )
        .with_pkce(false);
        let (url, _flow) = p.begin();
        assert!(!url.contains("code_challenge"));
    }

    #[test]
    fn begin_appends_extra_params() {
        let p = OAuth2Provider::new(
            "test",
            "cid",
            "csec",
            "https://app.test/cb",
            "https://idp.test/auth",
            "https://idp.test/token",
        )
        .with_extra_auth_params([("prompt", "consent"), ("access_type", "offline")]);
        let (url, _flow) = p.begin();
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("access_type=offline"));
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    fn loopback() -> TargetPolicy {
        TargetPolicy::Public(crate::outbound::Allowlist::parse("127.0.0.1"))
    }

    #[tokio::test]
    async fn discovery_refuses_a_private_issuer() {
        let issuer = serve(axum::Router::new()).await;
        let err = OAuth2Provider::new("x", "c", "s", "https://app/cb", "", "")
            .discover(&issuer, &TargetPolicy::public_only())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked address"), "{err}");
    }

    /// A provider whose token URL redirects to `/internal`; returns the hit count.
    async fn redirecting_provider() -> (OAuth2Provider, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new()
            .route(
                "/token",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, "/internal")],
                    )
                }),
            )
            .route(
                "/internal",
                axum::routing::post(move || {
                    h.fetch_add(1, Ordering::SeqCst);
                    async { "{}" }
                }),
            );
        let base = serve(app).await;
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", format!("{base}/token"));
        (p, hits)
    }

    #[tokio::test]
    async fn token_redirect_is_not_followed() {
        let (p, hits) = redirecting_provider().await;
        let (_, flow) = p.begin();
        let err = p
            .complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap_err();
        assert!(
            matches!(err, OAuthError::BadStatus { status: 307, .. }),
            "{err}"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn client_config_cannot_turn_redirects_back_on() {
        let (p, hits) = redirecting_provider().await;
        let p = p.with_client_config(|b| b.redirect(reqwest::redirect::Policy::limited(5)));
        let (_, flow) = p.begin();
        let err = p
            .complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap_err();
        assert!(
            matches!(err, OAuthError::BadStatus { status: 307, .. }),
            "{err}"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn client_config_is_applied() {
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(|h: axum::http::HeaderMap| async move {
                let ok = h.get("x-hook").is_some_and(|v| v == "1");
                (
                    axum::http::StatusCode::from_u16(if ok { 418 } else { 400 }).unwrap(),
                    "",
                )
            }),
        );
        let base = serve(app).await;
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", format!("{base}/token"))
            .with_client_config(|b| {
                let mut h = reqwest::header::HeaderMap::new();
                h.insert("x-hook", reqwest::header::HeaderValue::from_static("1"));
                b.default_headers(h)
            });
        let (_, flow) = p.begin();
        let err = p
            .complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap_err();
        assert!(
            matches!(err, OAuthError::BadStatus { status: 418, .. }),
            "{err}"
        );
    }

    /// Serves `/token` and `/userinfo`; the set holds each caller's peer address.
    async fn peer_recording_provider() -> (
        OAuth2Provider,
        Arc<std::sync::Mutex<std::collections::HashSet<std::net::SocketAddr>>>,
    ) {
        use axum::extract::ConnectInfo;
        let peers = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let (p1, p2) = (peers.clone(), peers.clone());
        let app = axum::Router::new()
            .route(
                "/token",
                axum::routing::post(move |ConnectInfo(a): ConnectInfo<std::net::SocketAddr>| {
                    p1.lock().unwrap().insert(a);
                    async { r#"{"access_token":"t"}"# }
                }),
            )
            .route(
                "/userinfo",
                axum::routing::get(move |ConnectInfo(a): ConnectInfo<std::net::SocketAddr>| {
                    p2.lock().unwrap().insert(a);
                    async { r#"{"sub":"u1"}"# }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let svc = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, svc).await.unwrap() });
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", format!("{base}/token"))
            .with_userinfo_url(format!("{base}/userinfo"));
        (p, peers)
    }

    /// The GitHub preset reads the verified flag from `/user/emails` (#1842).
    #[tokio::test]
    async fn github_flow_fetches_user_emails() {
        let app = axum::Router::new()
            .route(
                "/token",
                axum::routing::post(|| async { r#"{"access_token":"t"}"# }),
            )
            .route(
                "/user",
                axum::routing::get(|| async { r#"{"id":7,"email":"pub@example.com"}"# }),
            )
            .route(
                "/user/emails",
                axum::routing::get(|| async {
                    r#"[{"email":"pub@example.com","primary":true,"verified":true}]"#
                }),
            );
        let base = serve(app).await;
        let mut p = providers::github("c", "s", "https://app/cb")
            .with_userinfo_url(format!("{base}/user"))
            .with_emails_url(format!("{base}/user/emails"));
        p.token_url = format!("{base}/token");
        let (_, flow) = p.begin();
        let (user, _) = p
            .complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap();
        assert_eq!(user.email.as_deref(), Some("pub@example.com"));
        assert!(user.email_verified);
    }

    /// `/user/emails` refused (no `user:email` grant): the login goes on
    /// with an unverified email instead of a 502.
    #[tokio::test]
    async fn github_emails_403_or_404_means_unverified() {
        for status in [
            axum::http::StatusCode::FORBIDDEN,
            axum::http::StatusCode::NOT_FOUND,
        ] {
            let app = axum::Router::new()
                .route(
                    "/token",
                    axum::routing::post(|| async { r#"{"access_token":"t"}"# }),
                )
                .route(
                    "/user",
                    axum::routing::get(|| async { r#"{"id":7,"email":"pub@example.com"}"# }),
                )
                .route(
                    "/user/emails",
                    axum::routing::get(move || async move { (status, "denied") }),
                );
            let base = serve(app).await;
            let mut p = providers::github("c", "s", "https://app/cb")
                .with_userinfo_url(format!("{base}/user"))
                .with_emails_url(format!("{base}/user/emails"));
            p.token_url = format!("{base}/token");
            let (_, flow) = p.begin();
            let (user, _) = p
                .complete_with(&flow, "code", &flow.state, &loopback())
                .await
                .unwrap();
            assert_eq!(user.email.as_deref(), Some("pub@example.com"));
            assert!(!user.email_verified, "{status}");
        }
    }

    /// Token and userinfo calls reuse one pooled connection (#1792).
    #[tokio::test]
    async fn calls_reuse_one_connection() {
        let (p, peers) = peer_recording_provider().await;
        let (_, flow) = p.begin();
        p.complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap();
        assert_eq!(peers.lock().unwrap().len(), 1);

        let (p, peers) = peer_recording_provider().await;
        let p = p.with_client_config(|b| b);
        let (_, flow) = p.begin();
        p.complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap();
        assert_eq!(peers.lock().unwrap().len(), 1);
    }

    /// The shared client gets the 15 s timeout per request; a hook's
    /// client keeps its own.
    #[tokio::test]
    async fn timeouts_follow_the_client_kind() {
        const URL: &str = "http://127.0.0.1:9/token";
        async fn timeout(p: &OAuth2Provider) -> Option<Duration> {
            let req = p.checked(URL, reqwest::Method::POST, &loopback()).await;
            req.unwrap().build().unwrap().timeout().copied()
        }
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", URL);
        assert_eq!(timeout(&p).await, Some(HTTP_TIMEOUT));
        let p = p.with_client_config(|b| b);
        assert_eq!(timeout(&p).await, None);
    }

    /// Calling `with_client_config` again replaces the cached client.
    #[tokio::test]
    async fn new_client_config_resets_the_cache() {
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(|h: axum::http::HeaderMap| async move {
                let v = h
                    .get("x-hook")
                    .map_or(0, |v| v.to_str().unwrap().parse().unwrap());
                (axum::http::StatusCode::from_u16(400 + v).unwrap(), "")
            }),
        );
        let base = serve(app).await;
        let hook = |v: &'static str| {
            move |b: reqwest::ClientBuilder| {
                let mut h = reqwest::header::HeaderMap::new();
                h.insert("x-hook", reqwest::header::HeaderValue::from_static(v));
                b.default_headers(h)
            }
        };
        async fn status(p: &OAuth2Provider) -> u16 {
            let (_, flow) = p.begin();
            match p.complete_with(&flow, "c", &flow.state, &loopback()).await {
                Err(OAuthError::BadStatus { status, .. }) => status,
                other => panic!("{other:?}"),
            }
        }
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", format!("{base}/token"))
            .with_client_config(hook("1"));
        assert_eq!(status(&p).await, 401);
        let p = p.with_client_config(hook("2"));
        assert_eq!(status(&p).await, 402);
    }

    #[tokio::test]
    async fn token_error_body_is_bounded() {
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(|| async {
                (axum::http::StatusCode::BAD_REQUEST, "x".repeat(1_000_000))
            }),
        );
        let base = serve(app).await;
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", format!("{base}/token"));
        let (_, flow) = p.begin();
        let err = p
            .complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap_err();
        match err {
            OAuthError::BadStatus { status: 400, body } => {
                assert_eq!(body.len(), ERROR_BODY_MAX);
            }
            other => panic!("{other}"),
        }
    }

    /// Valid JSON padded past [`SUCCESS_BODY_MAX`].
    fn huge_json(fields: &str) -> String {
        format!(r#"{{{fields},"pad":"{}"}}"#, "x".repeat(SUCCESS_BODY_MAX))
    }

    /// Success bodies are capped too, even when they are valid JSON (#1793).
    #[tokio::test]
    async fn success_bodies_are_bounded() {
        let token = huge_json(r#""access_token":"a","token_type":"bearer""#);
        let doc = huge_json(r#""authorization_endpoint":"a","token_endpoint":"t""#);
        let app = axum::Router::new()
            .route("/token", axum::routing::post(move || async move { token }))
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(move || async move { doc }),
            );
        let base = serve(app).await;
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", format!("{base}/token"));
        let (_, flow) = p.begin();
        let err = p
            .complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, OAuthError::BadResponse(m) if m.contains("exceeds")),
            "{err}"
        );
        let err = OAuth2Provider::new("x", "c", "s", "https://app/cb", "", "")
            .discover(&base, &loopback())
            .await
            .err()
            .expect("discovery must fail");
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    /// A chunked body with no Content-Length is capped as it streams.
    #[tokio::test]
    async fn streamed_success_bodies_are_bounded() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = sock.read(&mut [0; 4096]).await;
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                transfer-encoding: chunked\r\n\r\n";
                    let _ = sock.write_all(head.as_bytes()).await;
                    let chunk = format!("10000\r\n{}\r\n", " ".repeat(0x10000));
                    for _ in 0..=SUCCESS_BODY_MAX / 0x10000 {
                        if sock.write_all(chunk.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                });
            }
        });
        let resp = reqwest::get(format!("{base}/x")).await.unwrap();
        assert_eq!(resp.content_length(), None, "the body must be streamed");
        let p = OAuth2Provider::new("t", "c", "s", "https://app/cb", "", format!("{base}/token"));
        let (_, flow) = p.begin();
        let err = p
            .complete_with(&flow, "code", &flow.state, &loopback())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn complete_refuses_a_metadata_token_url() {
        let p = OAuth2Provider::new(
            "t",
            "cid",
            "csec",
            "https://app.test/cb",
            "https://idp.test/auth",
            "http://169.254.169.254/token",
        );
        let (_, flow) = p.begin();
        let allow = crate::outbound::Allowlist::parse("10.0.5.0/24,idp.internal");
        let err = p
            .complete_with(&flow, "code", &flow.state, &TargetPolicy::Public(allow))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked address"), "{err}");
    }

    #[tokio::test]
    async fn complete_rejects_state_mismatch() {
        let p = OAuth2Provider::new(
            "test",
            "cid",
            "csec",
            "https://app.test/cb",
            "https://idp.test/auth",
            "https://idp.test/token",
        );
        let flow = OAuth2Flow {
            state: "expected".into(),
            pkce_verifier: "v".into(),
            created_at: 0,
        };
        let err = p.complete(&flow, "code", "wrong-state").await.unwrap_err();
        assert!(matches!(err, OAuthError::StateMismatch));
    }

    #[tokio::test]
    async fn complete_rejects_stale_flow() {
        // Audit N7 — a matching state but an ancient `created_at` is
        // rejected as FlowExpired *before* any token-exchange HTTP call.
        let p = OAuth2Provider::new(
            "test",
            "cid",
            "csec",
            "https://app.test/cb",
            "https://idp.test/auth",
            "https://idp.test/token",
        );
        let flow = OAuth2Flow {
            state: "expected".into(),
            pkce_verifier: "v".into(),
            created_at: 1, // ~1970 — far older than MAX_FLOW_AGE_SECS
        };
        // State matches, so we get past the CSRF check and hit the
        // freshness gate (no network).
        let err = p.complete(&flow, "code", "expected").await.unwrap_err();
        assert!(matches!(err, OAuthError::FlowExpired), "got {err:?}");
    }

    #[test]
    fn default_user_mapper_extracts_oidc_claims() {
        let raw = serde_json::json!({
            "sub": "12345",
            "email": "alice@example.com",
            "email_verified": true,
            "name": "Alice",
            "picture": "https://example.com/a.png",
        });
        let tokens = TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        };
        let u = default_user_mapper("google", raw, &tokens).unwrap();
        assert_eq!(u.provider, "google");
        assert_eq!(u.provider_user_id, "12345");
        assert_eq!(u.email.as_deref(), Some("alice@example.com"));
        assert!(u.email_verified);
        assert_eq!(u.name.as_deref(), Some("Alice"));
        assert_eq!(u.avatar_url.as_deref(), Some("https://example.com/a.png"));
    }

    #[test]
    fn default_user_mapper_errors_without_sub() {
        let raw = serde_json::json!({"email": "x@y.z"});
        let tokens = TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        };
        let err = default_user_mapper("p", raw, &tokens).unwrap_err();
        assert!(matches!(err, OAuthError::MissingField("sub")));
    }

    #[test]
    fn seal_and_open_round_trip() {
        let secret = b"shared-secret-key";
        let flow = OAuth2Flow {
            state: "s".into(),
            pkce_verifier: "v".into(),
            created_at: 1234,
        };
        let sealed = seal_flow(&flow, secret);
        let opened = open_flow(&sealed, secret).unwrap();
        assert_eq!(opened.state, "s");
        assert_eq!(opened.pkce_verifier, "v");
        assert_eq!(opened.created_at, 1234);
    }

    #[test]
    fn open_flow_rejects_tampering() {
        let secret = b"k";
        let flow = OAuth2Flow {
            state: "s".into(),
            pkce_verifier: "v".into(),
            created_at: 0,
        };
        let mut sealed = seal_flow(&flow, secret);
        // Flip a payload character (before the `.`) — sig should mismatch.
        let dot = sealed.find('.').unwrap();
        let tampered: String = sealed
            .char_indices()
            .map(|(i, c)| if i == dot - 1 { 'A' } else { c })
            .collect();
        sealed = tampered;
        let err = open_flow(&sealed, secret).unwrap_err();
        assert!(matches!(err, OAuthError::StateMismatch));
    }

    #[test]
    fn open_flow_rejects_wrong_secret() {
        let flow = OAuth2Flow {
            state: "s".into(),
            pkce_verifier: "v".into(),
            created_at: 0,
        };
        let sealed = seal_flow(&flow, b"key-a");
        let err = open_flow(&sealed, b"key-b").unwrap_err();
        assert!(matches!(err, OAuthError::StateMismatch));
    }

    #[test]
    fn debug_redacts_secret() {
        let p = OAuth2Provider::new(
            "test",
            "cid",
            "supersecret",
            "https://app.test/cb",
            "https://idp.test/auth",
            "https://idp.test/token",
        );
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("supersecret"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn provider_key_helper() {
        let k = provider_key("acme", "google");
        assert_eq!(k, ("acme".to_owned(), "google".to_owned()));
    }
}
