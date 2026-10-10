//! Member (end-user) social SSO — OpenID Connect / social OAuth login
//! for a **tenant's own user pool** (`rustango_users`), with optional
//! auto-provisioning.
//!
//! This is the member-facing analogue of the admin/tenant-console SSO
//! ([`crate::tenancy::sso`], [`crate::admin::sso`]). It reuses the exact
//! same admin-INDEPENDENT [`crate::sso`] OAuth2 core and the DB-backed
//! [`SsoProvider`] rows that already live in each tenant's storage — the
//! difference is the *session it mints*. Because the core lives in the
//! `sso` feature (not `admin-sso`), member SSO builds with just
//! `tenancy + sso` — no auto-admin required. Like the admin flows it signs
//! in by `(provider, sub)` link; unlike them it can **auto-provision** (and
//! link) a new tenant user when no account has the verified IdP email.
//!
//! [`SsoProvider`]: crate::sso::SsoProvider
//!
//! ## Two routes, per-slug (matches the tenant-SSO shape)
//!
//! * `GET {login_base}/sso/{slug}` — begin the handshake, redirect to
//!   the IdP.
//! * `GET {login_base}/sso/{slug}/callback` — complete the handshake,
//!   find-or-provision the member, mint the member session cookie.
//!
//! Mount the router returned by [`member_sso_router`] into a
//! [`crate::server::Builder`] stack (it reads the `TenantContext`
//! extension the builder injects — the flow is mount-agnostic, so no
//! separate `SessionSecret` extension is required).
//!
//! ## Session cookie — domain-separated from admin/tenant cookies
//!
//! The member cookie (`rustango_member_session`) is
//! **security-critically** domain-separated from the tenant-console
//! cookie: the signed message is prefixed with a per-domain tag, so a
//! member cookie can never validate as a tenant/admin cookie and
//! vice-versa even though both are signed with the same
//! `RUSTANGO_SESSION_SECRET`. Same wire format
//! (`<base64url(payload)>.<base64url(hmac)>`), same tenant-slug binding.
//!
//! ## v1 scope / trims
//!
//! * The `provision` closure hook is intentionally **not** exposed —
//!   v1 uses the built-in default provisioning in `provision_member`.
//! * Providers are resolved from the tenant's own [`SsoProvider`] rows
//!   only; the registry-wide shared-provider merge
//!   ([`crate::tenancy::sso::SharedSsoProvider`]) is a follow-up.

use std::convert::Infallible;

use axum::body::Body;
use axum::extract::{Extension, FromRequestParts, Path, Query, Request};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::extractors::{Tenant, TenantScope};
use crate::session::{secure_cookies, sign, PasswordFingerprint, SessionSecret};
use crate::sql::sqlx::Database;
use crate::sql::{Auto, Pool};
use crate::sso::link::{
    create_link_tx, sign_in, Account, AccountLookup, EmailLookup, LinkRefusal, LinkSource,
    ProviderKey,
};
use crate::sso::provider::resolve_by_slug;
use crate::sso::{build_provider, open_flow, seal_flow, FlowPurpose, FlowScope, NormalizedUser};
use crate::tenancy::DefaultTenantDb;
use crate::tenancy::User;

// ===================================================================
// A. Member session codec — SECURITY-CRITICAL, domain-separated.
// ===================================================================

/// Member session cookie name. Distinct from `rustango_tenant_session`
/// and `rustango_op_session` so a host serving both member and admin
/// UIs never collides the two.
pub const MEMBER_COOKIE: &str = "rustango_member_session";

/// Domain-separation tag mixed into every member signature. A cookie
/// signed for one domain (member) can never validate under another
/// (tenant/admin) even though they share the HMAC key — the tag makes
/// the signed message disjoint. Bump the `-v1` suffix to force a
/// global member-session invalidation on a breaking payload change.
const MEMBER_DOMAIN_TAG: &[u8] = b"rustango-member-session-v1";

/// Cookie the sealed [`crate::sso::OAuth2Flow`] round-trips in
/// between the begin redirect and the IdP callback. Short-lived,
/// `SameSite=Lax` so it survives the top-level redirect back from the
/// IdP. Distinct from the admin/tenant flow cookies.
const FLOW_COOKIE: &str = "rustango_member_sso_flow";

/// Max lifetime of the transient SSO flow cookie (10 minutes) — matches
/// the OAuth2 core's `MAX_FLOW_AGE_SECS`.
const FLOW_TTL_SECS: i64 = 600;

/// Principal payload carried inside the member cookie. Compact field
/// names keep the cookie short. `aud` pins the audience to `"member"`
/// so a payload minted for another surface (were the signature ever to
/// collide) is still refused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemberSessionPayload {
    /// `rustango_users.id` in the tenant's storage.
    pub uid: i64,
    /// Tenant slug the cookie was minted for (cross-tenant replay guard).
    pub slug: String,
    /// Expiry as Unix seconds.
    pub exp: i64,
    /// Issued-at as Unix seconds. A session issued before
    /// `rustango_users.password_changed_at` is rejected.
    pub iat: i64,
    /// Audience tag — always `"member"` for this codec.
    pub aud: String,
    /// Fingerprint of the user's `password_hash` at login. Any password
    /// change makes it stop matching (#1338).
    #[serde(default)]
    pub pwf: PasswordFingerprint,
}

impl MemberSessionPayload {
    /// Mint a fresh member payload. `aud` is fixed to `"member"`; `iat`
    /// is now and `exp` is `iat + ttl_secs`. `pwf` is the
    /// [`PasswordFingerprint`] of the user's current hash.
    #[must_use]
    pub fn new(uid: i64, slug: impl Into<String>, ttl_secs: i64, pwf: PasswordFingerprint) -> Self {
        let iat = chrono::Utc::now().timestamp();
        Self {
            uid,
            slug: slug.into(),
            exp: iat + ttl_secs,
            iat,
            aud: "member".to_owned(),
            pwf,
        }
    }

    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Distinct decode failures for the member codec. Kept separate from
/// the shared [`crate::tenancy::session::SessionError`] because the
/// member codec adds an audience check ([`Self::WrongAudience`]).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemberSessionError {
    /// Bad split, non-base64, or bad JSON.
    #[error("member session cookie is malformed")]
    Malformed,
    /// HMAC mismatch (covers tampering, secret rotation, and — by
    /// construction — a cookie minted for a different domain).
    #[error("member session signature mismatch")]
    BadSignature,
    /// `exp` in the past.
    #[error("member session expired")]
    Expired,
    /// `payload.slug` doesn't match the resolved tenant.
    #[error("member session is bound to a different tenant")]
    WrongTenant,
    /// `payload.aud` isn't `"member"`.
    #[error("member session has the wrong audience")]
    WrongAudience,
}

/// Serialize and sign a member payload over the domain-tagged message.
#[must_use]
pub fn encode(secret: &SessionSecret, payload: &MemberSessionPayload) -> String {
    let json = serde_json::to_vec(payload).expect("payload serializes");
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let msg = [MEMBER_DOMAIN_TAG, b".", payload_b64.as_bytes()].concat();
    let sig = sign(secret, &msg);
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{payload_b64}.{sig_b64}")
}

/// Verify, deserialize, and tenant/audience-bind-check a member cookie
/// value. The signature is recomputed over the **same** domain-tagged
/// message [`encode`] produced, so a tenant/admin cookie fed here fails
/// with [`MemberSessionError::BadSignature`].
///
/// # Errors
/// * [`MemberSessionError::Malformed`] — bad split / base64 / JSON.
/// * [`MemberSessionError::BadSignature`] — HMAC mismatch.
/// * [`MemberSessionError::WrongAudience`] — `aud != "member"`.
/// * [`MemberSessionError::Expired`] — `exp` in the past.
/// * [`MemberSessionError::WrongTenant`] — `slug != expected_slug`.
pub fn decode(
    secret: &SessionSecret,
    expected_slug: &str,
    value: &str,
) -> Result<MemberSessionPayload, MemberSessionError> {
    let (payload_b64, sig_b64) = value.split_once('.').ok_or(MemberSessionError::Malformed)?;
    let msg = [MEMBER_DOMAIN_TAG, b".", payload_b64.as_bytes()].concat();
    let expected = sign(secret, &msg);
    let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| MemberSessionError::Malformed)?;
    if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
        return Err(MemberSessionError::BadSignature);
    }
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| MemberSessionError::Malformed)?;
    let payload: MemberSessionPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| MemberSessionError::Malformed)?;
    if payload.aud != "member" {
        return Err(MemberSessionError::WrongAudience);
    }
    if payload.is_expired() {
        return Err(MemberSessionError::Expired);
    }
    if payload.slug != expected_slug {
        return Err(MemberSessionError::WrongTenant);
    }
    Ok(payload)
}

/// `"; Secure"` on the prod tier (HTTPS), empty in dev so local-HTTP SSO
/// works — the framework's session-cookie posture (audit H2), same as
/// the tenant login cookie ([`crate::tenancy::sso`]).
fn secure_suffix() -> &'static str {
    if secure_cookies() {
        "; Secure"
    } else {
        ""
    }
}

/// Build a `Set-Cookie` value minting a fresh member session for `user`
/// on `slug`, valid for `ttl` seconds. `HttpOnly; SameSite=Lax; Path=/`
/// with `; Secure` added on the prod tier. Changing the user's password
/// ends the session.
///
/// # Panics
/// If `user` has no id (was never saved).
#[must_use]
pub fn mint_cookie(secret: &SessionSecret, user: &User, slug: &str, ttl: i64) -> String {
    mint_cookie_at(secret, user, slug, ttl, "/")
}

/// [`mint_cookie`] scoped to `path`, a path-prefix tenant's prefix (#2098).
fn mint_cookie_at(secret: &SessionSecret, user: &User, slug: &str, ttl: i64, path: &str) -> String {
    let uid = user
        .id
        .get()
        .copied()
        .expect("mint_cookie needs a saved user row");
    let pwf = PasswordFingerprint::of(secret, &user.password_hash);
    let mut payload = MemberSessionPayload::new(uid, slug, ttl, pwf);
    payload.iat = crate::session::issued_at(user.sessions_revoked_at);
    let value = encode(secret, &payload);
    format!(
        "{MEMBER_COOKIE}={value}; HttpOnly; SameSite=Lax; Path={path}; Max-Age={ttl}{s}",
        s = secure_suffix(),
    )
}

/// Build a `Set-Cookie` value that expires the member session (logout).
/// Only clears this browser; [`logout`] also ends the other sessions.
/// `Path=/`: on a path-prefix tenant use [`logout_at`].
#[must_use]
pub fn clear_cookie() -> String {
    clear_cookie_at("/")
}

/// [`clear_cookie`] for a cookie minted with `Path={path}`.
fn clear_cookie_at(path: &str) -> String {
    format!("{MEMBER_COOKIE}=; HttpOnly; SameSite=Lax; Path={path}; Max-Age=0")
}

/// Log `user` out on every device and return the [`clear_cookie`] value.
/// Pass the row [`CurrentMember`] just loaded from the tenant `pool`.
/// On a path-prefix tenant use [`logout_at`], which clears the right path.
///
/// # Errors
/// [`crate::sql::ExecError`] when the cut-off cannot be stored.
pub async fn logout(pool: &crate::sql::Pool, user: &User) -> Result<String, crate::sql::ExecError> {
    revoke_member_sessions(pool, user).await?;
    Ok(clear_cookie())
}

/// [`logout`] whose cookie clears the path SSO minted it with: `org`'s path
/// prefix when `request_path` (the full path, e.g. `OriginalUri`) is under it (#2251).
///
/// # Errors
/// [`crate::sql::ExecError`] when the cut-off cannot be stored.
pub async fn logout_at(
    pool: &crate::sql::Pool,
    user: &User,
    org: &crate::tenancy::Org,
    request_path: &str,
) -> Result<String, crate::sql::ExecError> {
    revoke_member_sessions(pool, user).await?;
    Ok(Mount::at(org, request_path, "").clear_cookie())
}

async fn revoke_member_sessions(
    pool: &crate::sql::Pool,
    user: &User,
) -> Result<(), crate::sql::ExecError> {
    if let Some(&id) = user.id.get() {
        // No cookie here, so no skew cover: the cut-off is this node's clock.
        crate::session::revoke_sessions::<User>(pool, id, user.sessions_revoked_at, 0).await?;
    }
    Ok(())
}

// ===================================================================
// B. `CurrentMember` extractor — member analogue of `SessionUser`.
// ===================================================================

/// Reads the `rustango_member_session` cookie and returns the
/// corresponding active [`User`] row, or `None` for anonymous / expired
/// / rotated-out sessions. Infallible (`Rejection = Infallible`) so it
/// composes with public routes.
///
/// Mirrors [`crate::extractors::SessionUser`] but on the member codec
/// (slug-bound, audience-checked) and the member cookie. The resolved
/// org's slug validates the tenant binding — a member cookie minted for
/// `acme` never authenticates on `globex`.
pub struct CurrentMember(pub Option<User>);

impl<S: Send + Sync> FromRequestParts<S> for CurrentMember {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        use crate::tenancy::middleware::{request_org, request_pool, session_keys};

        let Some(keys) = session_keys(&parts.extensions) else {
            return Ok(CurrentMember(None));
        };

        let org = match request_org(parts, &parts.extensions).await {
            Some(Ok(Some(o))) => o,
            _ => return Ok(CurrentMember(None)),
        };

        let cookie_value = match extract_cookie(parts, MEMBER_COOKIE) {
            Some(v) => v,
            None => return Ok(CurrentMember(None)),
        };

        let payload = match decode(keys.session, &org.slug, &cookie_value) {
            Ok(p) => p,
            Err(_) => return Ok(CurrentMember(None)),
        };

        let pool = match request_pool(&parts.extensions, &org).await {
            Some(Ok(p)) => p,
            _ => return Ok(CurrentMember(None)),
        };

        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        let users = User::objects()
            .where_(User::id.eq(payload.uid))
            .fetch(&pool)
            .await
            .unwrap_or_default();

        let user = users.into_iter().next().filter(|u| u.active);
        // Reject a session minted before the user's last password
        // change or logout (parity with `SessionUser`).
        let user = user.filter(|u| {
            crate::tenancy::session::session_survives(
                keys.session,
                &payload.pwf,
                payload.iat,
                &u.password_hash,
                u.password_changed_at,
                u.sessions_revoked_at,
            )
        });
        Ok(CurrentMember(user))
    }
}

/// Pull one cookie value out of the `Cookie` request header by name.
fn extract_cookie(parts: &Parts, name: &str) -> Option<String> {
    crate::cookies::cookie_from_headers(&parts.headers, name).map(str::to_owned)
}

// ===================================================================
// C. Config.
// ===================================================================

/// Configuration for the member SSO router.
#[derive(Clone)]
pub struct MemberAuthConfig {
    /// Path the SSO routes hang off — buttons link to
    /// `{login_base}/sso/{slug}`. Default `"/auth"`.
    pub login_base: String,
    /// Post-login destination when no (sanitized) `?next` is present.
    /// Default `"/"`.
    pub landing_url: String,
    /// Auto-create a tenant user from a verified IdP email the first
    /// time it's seen. When `false`, an unknown email is refused.
    /// Default `false`: the provider rows are shared with the tenant console.
    pub auto_provision: bool,
    /// Member session lifetime in seconds. Default `604800` (7 days).
    pub session_ttl: i64,
}

impl Default for MemberAuthConfig {
    fn default() -> Self {
        Self {
            login_base: "/auth".to_owned(),
            landing_url: "/".to_owned(),
            auto_provision: false,
            session_ttl: 7 * 24 * 60 * 60,
        }
    }
}

// ===================================================================
// D. Router + handlers.
// ===================================================================

/// Build the member SSO router: begin + per-slug callback. Mount into a
/// [`crate::server::Builder`] stack — the handlers read the
/// `TenantContext` extension for the resolved tenant, its scoped
/// pool, and the session secret.
#[must_use]
pub fn member_sso_router(config: MemberAuthConfig) -> Router<()> {
    member_sso_router_for::<DefaultTenantDb>(config)
}

/// [`member_sso_router`] for a `Tenant<DB>` other than the default, e.g.
/// SQLite in a build that also enables `postgres` (#1741).
#[must_use]
pub fn member_sso_router_for<DB>(config: MemberAuthConfig) -> Router<()>
where
    DB: Database,
    Tenant<DB>: FromRequestParts<()> + Send,
{
    let login = config.login_base.trim_end_matches('/').to_owned();
    let mut router = Router::new();
    // A path-prefix tenant reaches the same routes under its prefix (#2145).
    for mount in ["", "/{prefix}"] {
        router = router
            .route(
                &format!("{mount}{login}/sso/{{slug}}"),
                get(sso_begin::<DB>),
            )
            .route(
                &format!("{mount}{login}/sso/{{slug}}/callback"),
                get(sso_callback::<DB>),
            );
    }
    router.layer(Extension(config))
}

/// Path params; `prefix` is set on the path-prefix tenant routes.
#[derive(Deserialize)]
struct SsoPath {
    slug: String,
    prefix: Option<String>,
}

/// Where this request's member routes live: `/` or the tenant's path
/// prefix. Every URL and cookie path the handlers emit goes through it (#2145).
struct Mount {
    /// `/` or the prefix (`/acme`), no trailing slash.
    path: String,
    /// `login_base` without a trailing slash.
    login: String,
}

impl Mount {
    /// `None` when a prefixed route matched a path outside the prefix.
    fn of(t: &TenantScope, sso: &SsoPath, parts: &Parts, login_base: &str) -> Option<Self> {
        let mount = Self::at(&t.org, parts.uri.path(), login_base);
        (sso.prefix.is_none() || mount.path != "/").then_some(mount)
    }

    /// The mount `org` is served under at `request_path` (#2098).
    fn at(org: &crate::tenancy::Org, request_path: &str, login_base: &str) -> Self {
        Self {
            path: super::routes::cookie_path(org, request_path).to_owned(),
            login: login_base.trim_end_matches('/').to_owned(),
        }
    }

    /// Mint `user`'s member cookie under this mount.
    fn mint_cookie(&self, secret: &SessionSecret, user: &User, slug: &str, ttl: i64) -> String {
        mint_cookie_at(secret, user, slug, ttl, &self.path)
    }

    /// Expire the member cookie [`Self::mint_cookie`] set.
    fn clear_cookie(&self) -> String {
        clear_cookie_at(&self.path)
    }

    /// A site-absolute path as seen under this mount.
    fn url(&self, site_path: &str) -> String {
        if self.path == "/" {
            site_path.to_owned()
        } else {
            format!("{}{site_path}", self.path)
        }
    }

    fn login_url(&self) -> String {
        match self.url(&self.login) {
            u if u.is_empty() => "/".to_owned(),
            u => u,
        }
    }

    /// The absolute callback URL for a slug — must match at begin + callback.
    /// `None` when the host is not plain `host[:port]` (#2173).
    fn callback_uri(&self, parts: &Parts, slug: &str) -> Option<String> {
        Some(format!(
            "{}{}/sso/{slug}/callback",
            external_base(parts)?.trim_end_matches('/'),
            self.url(&self.login),
        ))
    }
}

/// Query params on the IdP callback (`?code=…&state=…` or `?error=…`),
/// plus an optional post-login `?next` (honored only when same-origin).
#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    next: Option<String>,
}

/// Best-effort external scheme+host for building absolute redirect URIs:
/// the request scheme + `Host`. `X-Forwarded-Proto` / `X-Forwarded-Host`
/// count only from a proxy named in `RealIpLayer::trust_proxies` (#1842).
/// Default scheme is `https` for non-local hosts, `http` for
/// `localhost` / `127.` so local plain-HTTP dev works.
///
/// The result MUST be byte-identical at begin and callback — the
/// `redirect_uri` is part of the OAuth2 signature the IdP validates.
/// `None` when the host is not plain `host[:port]` (#2173).
#[must_use]
pub(crate) fn external_base(parts: &Parts) -> Option<String> {
    let headers = &parts.headers;
    fn first(v: &str) -> &str {
        v.split(',').next().unwrap_or(v).trim()
    }
    let forwarded =
        |name: &str| crate::real_ip::trusted_forwarded(headers, &parts.extensions, name);

    let proto = forwarded("x-forwarded-proto").map(str::to_owned);

    let raw = forwarded("x-forwarded-host")
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(first)
        })
        .unwrap_or("localhost");
    let host = crate::urls::HostAuthority::parse(raw)?.to_string();

    let scheme = proto.unwrap_or_else(|| {
        if let Some(s) = parts.uri.scheme_str() {
            s.to_owned()
        } else if host.contains("localhost") || host.starts_with("127.") {
            "http".to_owned()
        } else {
            "https".to_owned()
        }
    });

    Some(format!("{scheme}://{host}"))
}

/// `GET {login_base}/sso/{slug}` — begin the OAuth2 flow, redirect to
/// the IdP, seal the flow into the transient flow cookie.
fn sso_begin<DB: Database>(
    t: Tenant<DB>,
    Path(sso): Path<SsoPath>,
    Extension(config): Extension<MemberAuthConfig>,
    req: Request,
) -> impl std::future::Future<Output = Response> + Send {
    sso_begin_in(t.into(), sso, config, req)
}

async fn sso_begin_in(
    t: TenantScope,
    sso: SsoPath,
    config: MemberAuthConfig,
    req: Request,
) -> Response {
    let (parts, _body) = req.into_parts();
    let Some(mount) = Mount::of(&t, &sso, &parts, &config.login_base) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let slug = sso.slug.as_str();
    let Some(secret) = session_secret(&parts) else {
        return sso_error("Sign-in is temporarily unavailable.", &mount);
    };
    let pool = t.pool();

    let Some(redirect_uri) = mount.callback_uri(&parts, slug) else {
        return sso_error("Sign-in is temporarily unavailable.", &mount);
    };
    let resolved = match resolve_by_slug(pool, slug, redirect_uri).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(slug, "SSO provider not found / disabled");
            return sso_error("That sign-in method is not available.", &mount);
        }
        Err(e) => {
            tracing::error!(error = %e, slug, "resolve_by_slug failed");
            return sso_error("Sign-in is temporarily unavailable.", &mount);
        }
    };

    let provider = match build_provider(&resolved.sso).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, slug, "build_provider failed");
            return sso_error("Sign-in is temporarily unavailable.", &mount);
        }
    };

    let (authorize_url, flow) = provider.begin();
    let sealed = seal_flow(
        &flow,
        FlowScope::new(FlowPurpose::Member, &t.org.slug, slug),
        secret.key(),
    );
    let flow_cookie = format!(
        "{FLOW_COOKIE}={sealed}; HttpOnly; SameSite=Lax; Path={path}; Max-Age={FLOW_TTL_SECS}{s}",
        path = mount.path,
        s = secure_suffix(),
    );
    redirect_with_cookie(&authorize_url, &flow_cookie)
}

/// `GET {login_base}/sso/{slug}/callback` — exchange the code,
/// find-or-provision the member, mint the member session cookie.
fn sso_callback<DB: Database>(
    t: Tenant<DB>,
    Path(sso): Path<SsoPath>,
    Extension(config): Extension<MemberAuthConfig>,
    Query(params): Query<CallbackParams>,
    req: Request,
) -> impl std::future::Future<Output = Response> + Send {
    sso_callback_in(t.into(), sso, config, params, req)
}

async fn sso_callback_in(
    t: TenantScope,
    sso: SsoPath,
    config: MemberAuthConfig,
    params: CallbackParams,
    req: Request,
) -> Response {
    let (parts, _body) = req.into_parts();
    let Some(mount) = Mount::of(&t, &sso, &parts, &config.login_base) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let slug = sso.slug.as_str();
    let clear_flow = |resp: Response| clear_flow(resp, &mount.path);
    let Some(secret) = session_secret(&parts) else {
        return sso_error("Sign-in is temporarily unavailable.", &mount);
    };

    if let Some(err) = params.error {
        tracing::warn!(slug, error = %err, "IdP returned an error");
        return sso_error("Sign-in was cancelled or denied.", &mount);
    }
    let (Some(code), Some(state)) = (params.code, params.state) else {
        return sso_error("Malformed sign-in response.", &mount);
    };

    let Some(sealed) = extract_cookie(&parts, FLOW_COOKIE) else {
        return sso_error("Your sign-in session expired. Please try again.", &mount);
    };
    let flow = match open_flow(
        &sealed,
        FlowScope::new(FlowPurpose::Member, &t.org.slug, slug),
        secret.key(),
    ) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "open_flow failed");
            return sso_error("Your sign-in session expired. Please try again.", &mount);
        }
    };

    let pool = t.pool();

    let Some(redirect_uri) = mount.callback_uri(&parts, slug) else {
        return clear_flow(sso_error("Sign-in is temporarily unavailable.", &mount));
    };
    let resolved = match resolve_by_slug(pool, slug, redirect_uri).await {
        Ok(Some(r)) => r,
        _ => return clear_flow(sso_error("That sign-in method is not available.", &mount)),
    };
    let provider = match build_provider(&resolved.sso).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "build_provider failed on callback");
            return clear_flow(sso_error("Sign-in is temporarily unavailable.", &mount));
        }
    };

    let normalized = match provider.complete(&flow, &code, &state).await {
        Ok((user, _tokens)) => user,
        Err(e) => {
            tracing::error!(error = %e, "oauth2 complete failed");
            return clear_flow(sso_error("Sign-in failed. Please try again.", &mount));
        }
    };

    let key = resolved.key(LinkSource::Tenant);
    let member_id = match find_or_provision_member(
        pool,
        &key,
        resolved.allow_email_link,
        &normalized,
        config.auto_provision,
    )
    .await
    {
        Ok(MemberSignIn::Member(id)) => id,
        Ok(MemberSignIn::NotLinked) => {
            return clear_flow(sso_error(
                "This sign-in is not linked to your account. Please contact your administrator.",
                &mount,
            ));
        }
        Ok(MemberSignIn::NoAccount) => {
            return clear_flow(sso_error(
                "There is no account for this sign-in. Please contact your administrator.",
                &mount,
            ));
        }
        Ok(MemberSignIn::Inactive) => {
            return clear_flow(sso_error(
                "This account is disabled. Please contact your administrator.",
                &mount,
            ));
        }
        Ok(MemberSignIn::Unverified) => {
            return clear_flow(sso_error(
                "Your identity provider did not return a verified email.",
                &mount,
            ));
        }
        Ok(_) => {
            return clear_flow(sso_error("Sign-in failed. Please try again.", &mount));
        }
        Err(e) => {
            tracing::error!(error = %e, "find-or-provision member failed");
            return clear_flow(sso_error("Could not complete sign-in.", &mount));
        }
    };

    let Some(member) = tenant_user(pool, member_id).await.filter(|u| u.active) else {
        tracing::warn!(member_id, "member missing or inactive after sign-in");
        return clear_flow(sso_error("Could not complete sign-in.", &mount));
    };
    let cookie = mount.mint_cookie(&secret, &member, &t.org.slug, config.session_ttl);
    let landing = safe_landing(params.next.as_deref(), &mount.url(&config.landing_url));
    clear_flow(redirect_with_cookie(&landing, &cookie))
}

/// The mounted context's session secret, whichever backend it is for.
fn session_secret(parts: &Parts) -> Option<SessionSecret> {
    crate::tenancy::middleware::session_keys(&parts.extensions).map(|k| k.session.clone())
}

/// What [`find_or_provision_member`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemberSignIn {
    /// Signed in (linked, email-linked, or newly provisioned) as this user id.
    Member(i64),
    /// An account has this email but may not be linked by it: email linking
    /// is off, the account is privileged, or the email collides with another.
    NotLinked,
    /// No account has this email, and provisioning is off.
    NoAccount,
    /// The account is inactive.
    Inactive,
    /// The IdP sent no verified email (and there is no link).
    Unverified,
    /// Refused for another reason, e.g. no subject.
    Refused,
}

/// The member linked to `(provider, profile.provider_user_id)`; else see
/// [`crate::sso::link::sign_in`]; else, with `auto_provision`, a new linked member.
///
/// Public so a native sign-in that verified its own ID token uses the same rule.
///
/// # Errors
/// Storage failures.
pub async fn find_or_provision_member(
    pool: &Pool,
    provider: &ProviderKey,
    allow_email_link: bool,
    profile: &NormalizedUser,
    auto_provision: bool,
) -> Result<MemberSignIn, String> {
    let refusal = match sign_in(
        pool,
        provider,
        allow_email_link,
        profile,
        &TenantAccounts(pool),
    )
    .await
    {
        Ok(id) => return Ok(MemberSignIn::Member(id)),
        Err(LinkRefusal::NoAccount(email)) if auto_provision => {
            return provision_member(pool, provider, &email, profile)
                .await
                .map(MemberSignIn::Member);
        }
        Err(LinkRefusal::Storage(e)) => return Err(e),
        Err(e) => e,
    };
    tracing::warn!(subject = %profile.provider_user_id, "member sso refused: {refusal}");
    Ok(match refusal {
        LinkRefusal::EmailLinkDisabled | LinkRefusal::Privileged | LinkRefusal::EmailCollides => {
            MemberSignIn::NotLinked
        }
        LinkRefusal::NoAccount(_) => MemberSignIn::NoAccount,
        LinkRefusal::Inactive => MemberSignIn::Inactive,
        LinkRefusal::Unverified => MemberSignIn::Unverified,
        _ => MemberSignIn::Refused,
    })
}

/// A tenant's users. Privileged = superuser, or holds any admin permission;
/// a permission read error counts as privileged.
pub(crate) struct TenantAccounts<'a>(pub &'a Pool);

impl TenantAccounts<'_> {
    async fn account(&self, user: &User) -> Result<Option<Account>, String> {
        let Some(user_id) = user.id.get().copied() else {
            return Ok(None);
        };
        let privileged = user.is_superuser || holds_permissions(self.0, user_id).await;
        Ok(Some(Account::new(user_id, privileged, user.active)))
    }
}

/// Whether `user_id` holds any permission; a read error counts as yes.
async fn holds_permissions(pool: &Pool, user_id: i64) -> bool {
    crate::tenancy::permissions::user_permissions_pool(user_id, pool)
        .await
        .map_or(true, |perms| !perms.is_empty())
}

/// Whether some active user with an email is not privileged, so email
/// linking could sign them in (#2359).
pub(crate) async fn any_email_linkable(pool: &Pool) -> Result<bool, crate::sql::ExecError> {
    let ids = crate::query::QuerySet::<User>::new()
        .filter("active", true)
        .filter("is_superuser", false)
        .filter("email__isnull", false)
        .exclude("email", "")
        .values_list_flat("id")
        .fetch::<i64>(pool)
        .await?;
    // Sign-in's own rule; stops at the first user without permissions.
    for id in ids {
        if !holds_permissions(pool, id).await {
            return Ok(true);
        }
    }
    Ok(false)
}

impl AccountLookup for TenantAccounts<'_> {
    async fn by_id(&self, id: i64) -> Result<Option<Account>, String> {
        match find_tenant_user(self.0, id)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(u) => self.account(&u).await,
            None => Ok(None),
        }
    }

    async fn by_email(&self, email: &str) -> Result<EmailLookup, String> {
        use crate::sql::FetcherPool as _;
        let rows = User::objects()
            .filter("email__iexact", email.to_owned())
            .fetch(self.0)
            .await
            .map_err(|e| format!("lookup: {e}"))?;
        match EmailLookup::pick(&rows, email, |u| u.email.as_deref()) {
            Ok(Some(u)) => Ok(self
                .account(u)
                .await?
                .map_or(EmailLookup::Missing, EmailLookup::Found)),
            Ok(None) => Ok(EmailLookup::Missing),
            Err(()) => Ok(EmailLookup::Collides),
        }
    }
}

/// The tenant user with this id; a driver error is an error, not "missing".
pub(crate) async fn find_tenant_user(
    pool: &Pool,
    id: i64,
) -> Result<Option<User>, crate::sql::ExecError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    Ok(User::objects()
        .where_(User::id.eq(id))
        .fetch(pool)
        .await?
        .into_iter()
        .next())
}

/// The tenant user with this id, if it can be read.
pub(crate) async fn tenant_user(pool: &Pool, id: i64) -> Option<User> {
    find_tenant_user(pool, id).await.ok().flatten()
}

/// Auto-create a tenant user from a verified IdP email. `password_hash`
/// is a **real, unusable** random Argon2 hash (never an empty string —
/// empty is a login footgun); SSO users can't password-login anyway.
/// The username is the email local-part, deduped on unique clash.
async fn provision_member(
    pool: &Pool,
    provider: &ProviderKey,
    email: &str,
    profile: &NormalizedUser,
) -> Result<i64, String> {
    let base = email
        .split('@')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(email)
        .to_owned();

    let display_name = profile.name.clone().unwrap_or_else(|| base.clone());
    let data = serde_json::json!({
        "display_name": display_name,
        "avatar_url": profile.avatar_url,
    });

    // Two attempts: the plain local-part first (query-then-pick), then a
    // suffixed variant if it's taken or lost an insert race.
    for attempt in 0..2 {
        let username = if attempt == 0 && !username_taken(pool, &base).await? {
            base.clone()
        } else {
            format!("{base}-{}", short_suffix())
        };

        let mut user = User {
            id: Auto::Unset,
            username,
            password_hash: crate::tenancy::password::hash_async(&random_unusable_secret())
                .await
                .map_err(|e| format!("hash: {e}"))?,
            email: Some(email.to_owned()),
            is_superuser: false,
            active: true,
            created_at: chrono::Utc::now(),
            data: data.clone(),
            password_changed_at: None,
            sessions_revoked_at: None,
        };

        // The user and its link commit together, or neither does.
        let mut tx = crate::sql::transaction_pool(pool)
            .await
            .map_err(|e| format!("begin: {e}"))?;
        match user.insert_tx(&mut tx).await {
            Ok(()) => {
                let id = user
                    .id
                    .get()
                    .copied()
                    .ok_or_else(|| "insert returned no id".to_owned())?;
                create_link_tx(&mut tx, provider, &profile.provider_user_id, id)
                    .await
                    .map_err(|e| format!("link: {e}"))?;
                tx.commit().await.map_err(|e| format!("commit: {e}"))?;
                return Ok(id);
            }
            Err(e) if attempt == 0 => {
                // Likely a username/email unique clash — retry once with
                // a suffixed username.
                tracing::debug!(error = %e, "member insert retry after conflict");
            }
            Err(e) => return Err(format!("insert: {e}")),
        }
    }
    Err("could not allocate a unique username".to_owned())
}

/// `true` when a `rustango_users.username` row already exists.
async fn username_taken(pool: &Pool, username: &str) -> Result<bool, String> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    let rows = User::objects()
        .where_(User::username.eq(username.to_owned()))
        .limit(1)
        .fetch(pool)
        .await
        .map_err(|e| format!("username lookup: {e}"))?;
    Ok(!rows.is_empty())
}

/// A short, URL-safe suffix for username de-duplication.
fn short_suffix() -> String {
    crate::tenancy::password::generate(6).to_ascii_lowercase()
}

/// A real 32-byte random secret (base64) to seed an *unusable* Argon2
/// hash for an SSO-only account.
fn random_unusable_secret() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    base64::engine::general_purpose::STANDARD.encode(buf)
}

/// Sanitize a `?next` redirect target: only a same-origin absolute
/// path is honored, else fall back to `landing`.
///
/// Delegates to `auth_decorators::safe_next`, the one hardened copy of
/// this rule. The hand-rolled version here checked `starts_with('/')`
/// and `!starts_with("//")` and accepted `/\evil.example/x`, which a
/// browser rewrites to `//evil.example/x` — and the result goes
/// straight into `LOCATION` via `redirect_with_cookie`. Found by the
/// ratchet added with the #1526 re-fix; the review of PR #1604 caught
/// the two sibling copies and missed this one.
fn safe_landing(next: Option<&str>, landing: &str) -> String {
    next.and_then(crate::auth_decorators::safe_next)
        .unwrap_or_else(|| landing.to_owned())
}

/// A `303 See Other` redirect carrying a single `Set-Cookie`.
fn redirect_with_cookie(location: &str, cookie: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location)
        .header(header::SET_COOKIE, cookie)
        .body(Body::empty())
        .expect("valid redirect response")
}

/// Append a flow-cookie-clearing `Set-Cookie` to a response.
fn clear_flow(mut resp: Response, path: &str) -> Response {
    resp.headers_mut().append(
        header::SET_COOKIE,
        format!("{FLOW_COOKIE}=; HttpOnly; SameSite=Lax; Path={path}; Max-Age=0")
            .parse()
            .expect("valid cookie"),
    );
    resp
}

/// Minimal self-contained HTML error page for SSO failures — no
/// template dependency, so it renders even when tenant templates are
/// missing. Links back to the login page under `mount`.
fn sso_error(message: &str, mount: &Mount) -> Response {
    let back = crate::text::html_escape(&mount.login_url());
    let message = crate::text::html_escape(message);
    // A nonce'd `<style>`, not a style attribute: strict CSP (#2144).
    let nonce = crate::csp_nonce::nonce_attr();
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Sign-in error</title>\
         <style{nonce}>body{{font-family:system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem}}</style>\
         <body><h1>Sign-in error</h1><p>{message}</p><p><a href=\"{back}\">Back to sign-in</a></p></body>"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .expect("valid response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::session::SessionError;
    use crate::tenancy::tenant_console;

    fn secret() -> SessionSecret {
        SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec())
    }

    fn fp() -> PasswordFingerprint {
        PasswordFingerprint::of(&secret(), "$argon2id$test")
    }

    /// #2560 — provisioning a user from any verified email is opt-in.
    #[test]
    fn member_sso_does_not_auto_provision_by_default() {
        assert!(!MemberAuthConfig::default().auto_provision);
    }

    /// #2144: the SSO error page passes a strict CSP: no style attribute,
    /// and its `<style>` carries the request's nonce.
    #[tokio::test]
    async fn sso_error_page_passes_a_strict_csp() {
        use crate::csp_nonce::{CspNonceLayer, CspNonceRouterExt as _, Nonce};
        use tower::ServiceExt as _;
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|axum::Extension(n): axum::Extension<Nonce>| async move {
                    let mount = Mount {
                        path: "/".into(),
                        login: "/login".into(),
                    };
                    let mut resp = sso_error("bad", &mount);
                    resp.headers_mut()
                        .insert("x-nonce", n.value().parse().unwrap());
                    resp
                }),
            )
            .csp_nonce(CspNonceLayer::default());
        let req = axum::http::Request::new(Body::empty());
        let resp = app.oneshot(req).await.unwrap();
        let nonce = resp.headers()["x-nonce"].to_str().unwrap().to_owned();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains(" style="), "{html}");
        assert!(
            html.contains(&format!(r#"<style nonce="{nonce}">"#)),
            "{html}"
        );
    }

    // ---- A. domain separation (security-critical) -------------------

    #[test]
    fn member_cookie_round_trips() {
        let s = secret();
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600, fp()));
        let back = decode(&s, "acme", &value).expect("round-trips");
        assert_eq!(back.uid, 7);
        assert_eq!(back.slug, "acme");
        assert_eq!(back.aud, "member");
    }

    #[test]
    fn member_cookie_never_validates_as_tenant_cookie() {
        // A member cookie fed to the tenant-console decoder must fail —
        // the domain tag makes the signed message disjoint, so the HMAC
        // never matches.
        let s = secret();
        let member_value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600, fp()));
        let err = tenant_console::decode(&s, "acme", &member_value).unwrap_err();
        assert!(
            matches!(err, SessionError::BadSignature),
            "member cookie must not validate as a tenant cookie, got {err:?}"
        );
    }

    #[test]
    fn tenant_cookie_never_validates_as_member_cookie() {
        let s = secret();
        let tenant_value = tenant_console::encode(
            &s,
            &tenant_console::TenantSessionPayload::new(7, "acme", 3600, fp()),
        );
        let err = decode(&s, "acme", &tenant_value).unwrap_err();
        assert!(
            matches!(err, MemberSessionError::BadSignature),
            "tenant cookie must not validate as a member cookie, got {err:?}"
        );
    }

    #[test]
    fn member_decode_rejects_wrong_slug() {
        let s = secret();
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600, fp()));
        assert_eq!(
            decode(&s, "globex", &value).unwrap_err(),
            MemberSessionError::WrongTenant
        );
    }

    #[test]
    fn member_decode_rejects_expired() {
        let s = secret();
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", -10, fp()));
        assert_eq!(
            decode(&s, "acme", &value).unwrap_err(),
            MemberSessionError::Expired
        );
    }

    #[test]
    fn member_decode_rejects_tampered_signature() {
        let s = secret();
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600, fp()));
        let (_, sig) = value.split_once('.').unwrap();
        let evil = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"uid":999,"slug":"acme","exp":9999999999,"iat":0,"aud":"member"}"#);
        let tampered = format!("{evil}.{sig}");
        assert_eq!(
            decode(&s, "acme", &tampered).unwrap_err(),
            MemberSessionError::BadSignature
        );
    }

    #[test]
    fn member_decode_rejects_wrong_audience() {
        let s = secret();
        // Hand-mint a payload with a non-"member" audience, correctly
        // signed under the member domain tag — only `aud` is wrong.
        let payload = MemberSessionPayload {
            uid: 7,
            slug: "acme".to_owned(),
            exp: chrono::Utc::now().timestamp() + 3600,
            iat: chrono::Utc::now().timestamp(),
            aud: "admin".to_owned(),
            pwf: fp(),
        };
        let value = encode(&s, &payload);
        assert_eq!(
            decode(&s, "acme", &value).unwrap_err(),
            MemberSessionError::WrongAudience
        );
    }

    #[test]
    fn member_decode_rejects_malformed() {
        let s = secret();
        assert_eq!(
            decode(&s, "acme", "not-a-cookie").unwrap_err(),
            MemberSessionError::Malformed
        );
    }

    // ---- external_base (pure) ---------------------------------------

    fn parts_with(headers: &[(&str, &str)]) -> Parts {
        let mut b = axum::http::Request::builder().uri("/auth/sso/google/callback");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap().into_parts().0
    }

    /// A client can't pick the `redirect_uri` host with forwarded headers (#1842).
    #[test]
    fn external_base_ignores_forwarded_headers_from_an_untrusted_peer() {
        let parts = parts_with(&[
            ("x-forwarded-proto", "http"),
            ("x-forwarded-host", "evil.example.com"),
            ("host", "gym.example.com"),
        ]);
        assert_eq!(
            external_base(&parts).as_deref(),
            Some("https://gym.example.com")
        );
    }

    #[test]
    fn external_base_honors_forwarded_headers_from_a_trusted_proxy() {
        let mut parts = parts_with(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "g.example.com"),
            ("host", "internal:8080"),
        ]);
        parts
            .extensions
            .insert(crate::real_ip::TrustedRealIp([203, 0, 113, 9].into()));
        assert_eq!(
            external_base(&parts).as_deref(),
            Some("https://g.example.com")
        );
    }

    #[test]
    fn external_base_localhost_defaults_to_http() {
        let parts = parts_with(&[("host", "downtown.localhost:8080")]);
        assert_eq!(
            external_base(&parts).as_deref(),
            Some("http://downtown.localhost:8080")
        );
    }

    #[test]
    fn external_base_public_host_defaults_to_https() {
        let parts = parts_with(&[("host", "gym.example.com")]);
        assert_eq!(
            external_base(&parts).as_deref(),
            Some("https://gym.example.com")
        );
    }

    /// The `redirect_uri` never takes userinfo from the Host (#2173).
    #[test]
    fn external_base_refuses_a_host_with_userinfo() {
        for host in ["good.com:1@evil.com:2", "good.com@evil.com"] {
            assert_eq!(
                external_base(&parts_with(&[("host", host)])),
                None,
                "{host}"
            );
        }
        let mut parts = parts_with(&[("x-forwarded-host", "a@evil.com"), ("host", "gym.test")]);
        parts
            .extensions
            .insert(crate::real_ip::TrustedRealIp([203, 0, 113, 9].into()));
        assert_eq!(external_base(&parts), None);
    }

    /// #2251 — logout clears the member cookie at the path SSO minted it with.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn logout_clears_the_cookie_where_it_was_minted() {
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::create_tables_for::<User>(&pool)
            .await
            .unwrap();
        let mut user = crate::testkit::user();
        user.insert_pool(&pool).await.unwrap();
        let org = crate::tenancy::Org {
            path_prefix: Some("/acme".into()),
            ..crate::testkit::org()
        };
        let path_of = |cookie: &str| {
            cookie
                .split("; ")
                .find_map(|a| a.strip_prefix("Path="))
                .map(str::to_owned)
        };
        let minted = Mount::at(&org, "/acme/auth/sso/corp/callback", "/auth").mint_cookie(
            &secret(),
            &user,
            &org.slug,
            60,
        );
        let cleared = logout_at(&pool, &user, &org, "/acme/logout").await.unwrap();
        assert_eq!(path_of(&minted).as_deref(), Some("/acme"));
        assert_eq!(path_of(&cleared), path_of(&minted));
    }

    // ---- safe_landing -----------------------------------------------

    #[test]
    fn safe_landing_only_allows_same_origin_paths() {
        assert_eq!(safe_landing(Some("/dashboard"), "/"), "/dashboard");
        assert_eq!(safe_landing(Some("//evil.com"), "/"), "/");
        assert_eq!(safe_landing(Some("https://evil.com"), "/"), "/");
        assert_eq!(safe_landing(None, "/home"), "/home");
        // The case the hand-rolled version accepted: starts with `/`,
        // is not `//`, and the browser rewrites `\` to `/` so it
        // leaves as protocol-relative `//evil.com/x` (#1526).
        assert_eq!(safe_landing(Some("/\\evil.com/x"), "/"), "/");
        assert_eq!(safe_landing(Some("/%5Cevil.com/x"), "/"), "/");
    }
}
