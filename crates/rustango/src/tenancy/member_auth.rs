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
//! `tenancy + sso` — no auto-admin required. Where the admin flow is
//! **link-to-existing** (an unknown email is refused), the member flow
//! can **auto-provision** a new tenant user from a verified IdP email so
//! a gym member / SaaS end-user can sign in the first time without an
//! operator creating the row by hand.
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
//! [`crate::server::Builder`] stack (it reads the `Arc<TenantContext>`
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
//!   v1 uses the built-in default provisioning in [`provision_member`].
//! * Providers are resolved from the tenant's own [`SsoProvider`] rows
//!   only; the registry-wide shared-provider merge
//!   ([`crate::tenancy::sso::SharedSsoProvider`]) is a follow-up.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, FromRequestParts, Path, Query, Request};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::extractors::{Tenant, TenantContext};
use crate::session::{secure_cookies, sign, SessionSecret};
use crate::sql::{Auto, Pool};
use crate::sso::provider::resolve_by_slug;
use crate::sso::{build_provider, open_flow, seal_flow, verified_email, NormalizedUser};
use crate::tenancy::{OrgResolver as _, User};

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
    /// Issued-at as Unix seconds. Compared against
    /// `rustango_users.password_changed_at` so a password rotation
    /// invalidates live member sessions (parity with `SessionUser`).
    pub iat: i64,
    /// Audience tag — always `"member"` for this codec.
    pub aud: String,
}

impl MemberSessionPayload {
    /// Mint a fresh member payload. `aud` is fixed to `"member"`; `iat`
    /// is now and `exp` is `iat + ttl_secs`.
    #[must_use]
    pub fn new(uid: i64, slug: impl Into<String>, ttl_secs: i64) -> Self {
        let iat = chrono::Utc::now().timestamp();
        Self {
            uid,
            slug: slug.into(),
            exp: iat + ttl_secs,
            iat,
            aud: "member".to_owned(),
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

/// Build a `Set-Cookie` value minting a fresh member session for `uid`
/// on `slug`, valid for `ttl` seconds. `HttpOnly; SameSite=Lax; Path=/`
/// with `; Secure` added on the prod tier.
#[must_use]
pub fn mint_cookie(secret: &SessionSecret, uid: i64, slug: &str, ttl: i64) -> String {
    let value = encode(secret, &MemberSessionPayload::new(uid, slug, ttl));
    format!(
        "{MEMBER_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={ttl}{s}",
        s = secure_suffix(),
    )
}

/// Build a `Set-Cookie` value that expires the member session (logout).
#[must_use]
pub fn clear_cookie() -> String {
    format!("{MEMBER_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
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
        let Some(ctx) = parts.extensions.get::<Arc<TenantContext>>().cloned() else {
            return Ok(CurrentMember(None));
        };

        let org = match ctx
            .resolver
            .resolve(parts, &ctx.pools.registry_pool())
            .await
        {
            Ok(Some(o)) => o,
            _ => return Ok(CurrentMember(None)),
        };

        let cookie_value = match extract_cookie(parts, MEMBER_COOKIE) {
            Some(v) => v,
            None => return Ok(CurrentMember(None)),
        };

        let payload = match decode(&ctx.session_secret, &org.slug, &cookie_value) {
            Ok(p) => p,
            Err(_) => return Ok(CurrentMember(None)),
        };

        let pool = match ctx.pools.scoped_pool_dyn(&org).await {
            Ok(p) => p,
            Err(_) => return Ok(CurrentMember(None)),
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
        // change (parity with `SessionUser`). `password_changed_at IS
        // NULL` (never rotated) stays valid.
        let user = user.filter(|u| match u.password_changed_at {
            Some(changed) => payload.iat >= changed.timestamp(),
            None => true,
        });
        Ok(CurrentMember(user))
    }
}

/// Pull one cookie value out of the `Cookie` request header by name.
fn extract_cookie(parts: &Parts, name: &str) -> Option<String> {
    let header = parts.headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(name) {
            if let Some(v) = val.strip_prefix('=') {
                return Some(v.to_owned());
            }
        }
    }
    None
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
    /// time it's seen. When `false`, an unknown email is refused (like
    /// the admin link-to-existing flow). Default `true`.
    pub auto_provision: bool,
    /// Member session lifetime in seconds. Default `604800` (7 days).
    pub session_ttl: i64,
}

impl Default for MemberAuthConfig {
    fn default() -> Self {
        Self {
            login_base: "/auth".to_owned(),
            landing_url: "/".to_owned(),
            auto_provision: true,
            session_ttl: 7 * 24 * 60 * 60,
        }
    }
}

// ===================================================================
// D. Router + handlers.
// ===================================================================

/// Build the member SSO router: begin + per-slug callback. Mount into a
/// [`crate::server::Builder`] stack — the handlers read the
/// `Arc<TenantContext>` extension for the resolved tenant, its scoped
/// pool, and the session secret.
#[must_use]
pub fn member_sso_router(config: MemberAuthConfig) -> Router<()> {
    let login = config.login_base.trim_end_matches('/').to_owned();
    let begin_path = format!("{login}/sso/{{slug}}");
    let callback_path = format!("{login}/sso/{{slug}}/callback");
    Router::new()
        .route(&begin_path, get(sso_begin))
        .route(&callback_path, get(sso_callback))
        .layer(Extension(config))
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

/// Best-effort external scheme+host for building absolute redirect URIs,
/// honoring `X-Forwarded-Proto` and `X-Forwarded-Host` (behind a proxy /
/// load balancer) and falling back to the request scheme + `Host`.
/// Default scheme is `https` for non-local hosts, `http` for
/// `localhost` / `127.` so local plain-HTTP dev works.
///
/// The result MUST be byte-identical at begin and callback — the
/// `redirect_uri` is part of the OAuth2 signature the IdP validates.
#[must_use]
pub(crate) fn external_base(parts: &Parts) -> String {
    let headers = &parts.headers;
    let first = |v: &str| v.split(',').next().unwrap_or(v).trim().to_owned();

    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(first)
        .filter(|s| !s.is_empty());

    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()))
        .map(first)
        .unwrap_or_else(|| "localhost".to_owned());

    let scheme = proto.unwrap_or_else(|| {
        if let Some(s) = parts.uri.scheme_str() {
            s.to_owned()
        } else if host.contains("localhost") || host.starts_with("127.") {
            "http".to_owned()
        } else {
            "https".to_owned()
        }
    });

    format!("{scheme}://{host}")
}

/// The absolute callback URL for a slug — must match at begin + callback.
fn callback_uri(parts: &Parts, login_base: &str, slug: &str) -> String {
    format!(
        "{}/{}/sso/{}/callback",
        external_base(parts).trim_end_matches('/'),
        login_base.trim_matches('/'),
        slug,
    )
}

/// `GET {login_base}/sso/{slug}` — begin the OAuth2 flow, redirect to
/// the IdP, seal the flow into the transient flow cookie.
async fn sso_begin(
    t: Tenant,
    Path(slug): Path<String>,
    Extension(ctx): Extension<Arc<TenantContext>>,
    Extension(config): Extension<MemberAuthConfig>,
    req: Request,
) -> Response {
    let (parts, _body) = req.into_parts();

    let pool = match ctx.pools.scoped_pool_dyn(&t.org).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, slug, "scoped pool build failed");
            return sso_error("Sign-in is temporarily unavailable.", &config.login_base);
        }
    };

    let redirect_uri = callback_uri(&parts, &config.login_base, &slug);
    let resolved = match resolve_by_slug(&pool, &slug, redirect_uri).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(slug, "SSO provider not found / disabled");
            return sso_error("That sign-in method is not available.", &config.login_base);
        }
        Err(e) => {
            tracing::error!(error = %e, slug, "resolve_by_slug failed");
            return sso_error("Sign-in is temporarily unavailable.", &config.login_base);
        }
    };

    let provider = match build_provider(&resolved).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, slug, "build_provider failed");
            return sso_error("Sign-in is temporarily unavailable.", &config.login_base);
        }
    };

    let (authorize_url, flow) = provider.begin();
    let sealed = seal_flow(&flow, ctx.session_secret.key());
    let flow_cookie = format!(
        "{FLOW_COOKIE}={sealed}; HttpOnly; SameSite=Lax; Path=/; Max-Age={FLOW_TTL_SECS}{s}",
        s = secure_suffix(),
    );
    redirect_with_cookie(&authorize_url, &flow_cookie)
}

/// `GET {login_base}/sso/{slug}/callback` — exchange the code,
/// find-or-provision the member, mint the member session cookie.
async fn sso_callback(
    t: Tenant,
    Path(slug): Path<String>,
    Extension(ctx): Extension<Arc<TenantContext>>,
    Extension(config): Extension<MemberAuthConfig>,
    Query(params): Query<CallbackParams>,
    req: Request,
) -> Response {
    let (parts, _body) = req.into_parts();
    let login_base = &config.login_base;

    if let Some(err) = params.error {
        tracing::warn!(slug, error = %err, "IdP returned an error");
        return sso_error("Sign-in was cancelled or denied.", login_base);
    }
    let (Some(code), Some(state)) = (params.code, params.state) else {
        return sso_error("Malformed sign-in response.", login_base);
    };

    let Some(sealed) = extract_cookie(&parts, FLOW_COOKIE) else {
        return sso_error(
            "Your sign-in session expired. Please try again.",
            login_base,
        );
    };
    let flow = match open_flow(&sealed, ctx.session_secret.key()) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "open_flow failed");
            return sso_error(
                "Your sign-in session expired. Please try again.",
                login_base,
            );
        }
    };

    let pool = match ctx.pools.scoped_pool_dyn(&t.org).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, slug, "scoped pool build failed");
            return clear_flow(sso_error("Sign-in is temporarily unavailable.", login_base));
        }
    };

    let redirect_uri = callback_uri(&parts, login_base, &slug);
    let resolved = match resolve_by_slug(&pool, &slug, redirect_uri).await {
        Ok(Some(r)) => r,
        _ => {
            return clear_flow(sso_error(
                "That sign-in method is not available.",
                login_base,
            ))
        }
    };
    let provider = match build_provider(&resolved).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "build_provider failed on callback");
            return clear_flow(sso_error("Sign-in is temporarily unavailable.", login_base));
        }
    };

    let normalized = match provider.complete(&flow, &code, &state).await {
        Ok((user, _tokens)) => user,
        Err(e) => {
            tracing::error!(error = %e, "oauth2 complete failed");
            return clear_flow(sso_error("Sign-in failed. Please try again.", login_base));
        }
    };

    let email = match verified_email(&normalized) {
        Ok(e) => e.to_ascii_lowercase(),
        Err(e) => {
            tracing::warn!(error = %e, "unverified / missing email from IdP");
            return clear_flow(sso_error(
                "Your identity provider did not return a verified email.",
                login_base,
            ));
        }
    };

    let member_id =
        match find_or_provision_member(&pool, &email, &normalized, config.auto_provision).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::warn!(email, "no member account and auto-provision disabled");
                return clear_flow(sso_error(
                    "There is no account for that email. Please contact your administrator.",
                    login_base,
                ));
            }
            Err(e) => {
                tracing::error!(error = %e, "find-or-provision member failed");
                return clear_flow(sso_error("Could not complete sign-in.", login_base));
            }
        };

    let cookie = mint_cookie(
        &ctx.session_secret,
        member_id,
        &t.org.slug,
        config.session_ttl,
    );
    let landing = safe_landing(params.next.as_deref(), &config.landing_url);
    clear_flow(redirect_with_cookie(&landing, &cookie))
}

/// Match the IdP email to an existing member, else auto-provision one
/// (when `auto_provision`). Returns `Ok(Some(id))` on match/create,
/// `Ok(None)` when unknown and auto-provision is off (caller refuses).
async fn find_or_provision_member(
    pool: &Pool,
    email: &str,
    profile: &NormalizedUser,
    auto_provision: bool,
) -> Result<Option<i64>, String> {
    use crate::sql::FetcherPool as _;

    // Idempotent — find by (lowercased) email first.
    let existing = User::objects()
        .filter("email", email.to_owned())
        .fetch(pool)
        .await
        .map_err(|e| format!("lookup: {e}"))?
        .into_iter()
        .next();
    if let Some(u) = existing {
        return Ok(Some(
            u.id.get()
                .copied()
                .ok_or_else(|| "existing user missing id".to_owned())?,
        ));
    }

    if !auto_provision {
        return Ok(None);
    }

    provision_member(pool, email, profile).await.map(Some)
}

/// Auto-create a tenant user from a verified IdP email. `password_hash`
/// is a **real, unusable** random Argon2 hash (never an empty string —
/// empty is a login footgun); SSO users can't password-login anyway.
/// The username is the email local-part, deduped on unique clash.
async fn provision_member(
    pool: &Pool,
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
            password_hash: crate::tenancy::password::hash(&random_unusable_secret())
                .map_err(|e| format!("hash: {e}"))?,
            email: Some(email.to_owned()),
            is_superuser: false,
            active: true,
            created_at: chrono::Utc::now(),
            data: data.clone(),
            password_changed_at: None,
        };

        match user.insert_pool(pool).await {
            Ok(()) => {
                return user
                    .id
                    .get()
                    .copied()
                    .ok_or_else(|| "insert returned no id".to_owned());
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

/// Sanitize a `?next` redirect target: only a same-origin absolute path
/// (`/...`, not `//host`) is honored, else fall back to `landing`.
fn safe_landing(next: Option<&str>, landing: &str) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => n.to_owned(),
        _ => landing.to_owned(),
    }
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
fn clear_flow(mut resp: Response) -> Response {
    resp.headers_mut().append(
        header::SET_COOKIE,
        format!("{FLOW_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
            .parse()
            .expect("valid cookie"),
    );
    resp
}

/// Minimal self-contained HTML error page for SSO failures — no
/// template dependency, so it renders even when tenant templates are
/// missing. Links back to `login_base`.
fn sso_error(message: &str, login_base: &str) -> Response {
    let back = if login_base.is_empty() {
        "/"
    } else {
        login_base
    };
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Sign-in error</title>\
         <body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem\">\
         <h1>Sign-in error</h1><p>{message}</p><p><a href=\"{back}\">Back to sign-in</a></p></body>"
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

    // ---- A. domain separation (security-critical) -------------------

    #[test]
    fn member_cookie_round_trips() {
        let s = secret();
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600));
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
        let member_value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600));
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
            &tenant_console::TenantSessionPayload::new(7, "acme", 3600),
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
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600));
        assert_eq!(
            decode(&s, "globex", &value).unwrap_err(),
            MemberSessionError::WrongTenant
        );
    }

    #[test]
    fn member_decode_rejects_expired() {
        let s = secret();
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", -10));
        assert_eq!(
            decode(&s, "acme", &value).unwrap_err(),
            MemberSessionError::Expired
        );
    }

    #[test]
    fn member_decode_rejects_tampered_signature() {
        let s = secret();
        let value = encode(&s, &MemberSessionPayload::new(7, "acme", 3600));
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

    #[test]
    fn external_base_honors_forwarded_headers() {
        let parts = parts_with(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "g.example.com"),
            ("host", "internal:8080"),
        ]);
        assert_eq!(external_base(&parts), "https://g.example.com");
    }

    #[test]
    fn external_base_localhost_defaults_to_http() {
        let parts = parts_with(&[("host", "downtown.localhost:8080")]);
        assert_eq!(external_base(&parts), "http://downtown.localhost:8080");
    }

    #[test]
    fn external_base_public_host_defaults_to_https() {
        let parts = parts_with(&[("host", "gym.example.com")]);
        assert_eq!(external_base(&parts), "https://gym.example.com");
    }

    // ---- safe_landing -----------------------------------------------

    #[test]
    fn safe_landing_only_allows_same_origin_paths() {
        assert_eq!(safe_landing(Some("/dashboard"), "/"), "/dashboard");
        assert_eq!(safe_landing(Some("//evil.com"), "/"), "/");
        assert_eq!(safe_landing(Some("https://evil.com"), "/"), "/");
        assert_eq!(safe_landing(None, "/home"), "/home");
    }
}
