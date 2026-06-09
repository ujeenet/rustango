//! Built-in HTTP endpoints for JWT auth (#81).
//!
//! The framework already ships every primitive needed to issue and
//! verify JWTs against the per-tenant `rustango_users` table —
//! [`crate::tenancy::jwt_lifecycle::JwtLifecycle`] for the token
//! lifecycle, [`crate::tenancy::password::verify`] for the Argon2id
//! check, [`crate::tenancy::auth::User`] for the user row. But every
//! tenancy project re-implements the same `POST /api/auth/login`
//! handler, ~50 lines of boilerplate. This module is that handler,
//! plus the standard surface (refresh / logout / me).
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::tenancy::auth_routes;
//!
//! rustango::manage::Cli::new()
//!     .tenancy()
//!     .api(my_app::urls::api()
//!         .merge(auth_routes::jwt_router(auth_routes::Config::default())))
//!     .run().await
//! ```
//!
//! Endpoints mounted (paths configurable via [`Config`]):
//!
//! | Method | Path                  | Body / Auth                  | Returns |
//! |--------|-----------------------|------------------------------|---------|
//! | POST   | `/api/auth/login`     | `{username, password}`       | `{access, refresh, user}` |
//! | POST   | `/api/auth/refresh`   | `{refresh}`                  | `{access, refresh}` |
//! | POST   | `/api/auth/logout`    | `Authorization: Bearer ...`  | `204` (revokes jti) |
//! | GET    | `/api/auth/me`        | `Authorization: Bearer ...`  | `{user_id, username, is_superuser}` |
//!
//! Uses the framework's own `RUSTANGO_SESSION_SECRET` as the HMAC key
//! by default — same key as the admin session cookie. Override via
//! [`Config::session_secret`] for projects that want a separate
//! signing key.

use std::sync::OnceLock;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::extractors::Tenant;
use crate::tenancy::jwt_lifecycle::JwtLifecycle;

// ---------------------------------------------------------------- Config

/// Knobs for [`jwt_router`]. All have sensible defaults; override
/// when integrating with non-default URL prefixes (#74), shorter
/// access TTLs, custom signing keys, etc.
#[derive(Debug, Clone)]
pub struct Config {
    /// URL prefix every endpoint mounts under. Default `/api/auth`.
    pub prefix: String,
    /// Access token lifetime in seconds. Default 900 (15 min).
    pub access_ttl_secs: i64,
    /// Refresh token lifetime in seconds. Default 7 days.
    pub refresh_ttl_secs: i64,
    /// HMAC signing key. `None` (default) reads from the
    /// `RUSTANGO_SESSION_SECRET` env var so the framework's own
    /// session secret is reused. Set explicitly for projects that
    /// want separate signing keys for cookie sessions vs API JWTs.
    pub session_secret: Option<Vec<u8>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: "/api/auth".to_owned(),
            access_ttl_secs: 900,
            refresh_ttl_secs: 7 * 86400,
            session_secret: None,
        }
    }
}

/// Minimum HMAC key length (bytes) accepted for JWT signing. 32 bytes
/// matches the SHA-256 block/output size and `SessionSecret`'s own
/// floor. Shorter keys — in particular the **empty** key produced when
/// `RUSTANGO_SESSION_SECRET` is unset and no explicit `session_secret`
/// is given — are publicly guessable and would let anyone forge tokens
/// (audit C1 / GHSA-3g36-xq5c-8j45).
const MIN_HMAC_KEY_LEN: usize = 32;

impl Config {
    fn build_jwt(&self) -> JwtLifecycle {
        let secret = self.session_secret.clone().unwrap_or_else(|| {
            std::env::var("RUSTANGO_SESSION_SECRET")
                .unwrap_or_default()
                .into_bytes()
        });
        // Fail closed: never sign JWTs with an empty / too-short key.
        // A misconfigured deployment must refuse to start rather than
        // silently mint forgeable access + refresh tokens.
        assert!(
            secret.len() >= MIN_HMAC_KEY_LEN,
            "JWT signing key is {} bytes; need >= {MIN_HMAC_KEY_LEN}. Set \
             RUSTANGO_SESSION_SECRET to a base64-encoded 32+ byte value \
             (e.g. `openssl rand -base64 32`) or pass an explicit \
             auth_routes::Config::session_secret. Refusing to start with a \
             guessable key (would allow JWT forgery).",
            secret.len(),
        );
        JwtLifecycle::new(secret)
            .with_access_ttl(self.access_ttl_secs)
            .with_refresh_ttl(self.refresh_ttl_secs)
    }

    /// Apply values from a loaded [`crate::config::JwtSettings`]
    /// section (#87 wiring, v0.29). Each field is `Option`-typed in
    /// TOML — missing keys fall through to the existing `Config`
    /// defaults (15 min access, 7 days refresh) so partial config
    /// stays forward-compatible.
    ///
    /// Currently honors `access_ttl_secs` and `refresh_ttl_secs`.
    /// `issuer` and `audience` are accepted by the section but not
    /// yet threaded through `JwtLifecycle` — when that ships, the
    /// wiring lands here automatically.
    ///
    /// ```ignore
    /// let cfg = rustango::config::Settings::load_from_env()?;
    /// let auth = auth_routes::Config::default()
    ///     .with_jwt_settings(&cfg.auth.jwt);
    /// api.merge(auth_routes::jwt_router(auth))
    /// ```
    #[cfg(feature = "config")]
    #[must_use]
    pub fn with_jwt_settings(mut self, s: &crate::config::JwtSettings) -> Self {
        // u64 → i64 saturating conversion. Realistic TTLs cap out
        // around 7 days (refresh) or 1h (access); the saturate path
        // only trips for absurd configs (years), which would be
        // wrong for a different reason — flag-not-fatal.
        if let Some(v) = s.access_ttl_secs {
            self.access_ttl_secs = i64::try_from(v).unwrap_or(i64::MAX);
        }
        if let Some(v) = s.refresh_ttl_secs {
            self.refresh_ttl_secs = i64::try_from(v).unwrap_or(i64::MAX);
        }
        self
    }
}

// ---------------------------------------------------------------- The router

/// Module-local singleton: the first call to [`jwt_router`] wins.
/// Holds both the resolved [`Config`] and the built [`JwtLifecycle`]
/// so handlers can share the same in-memory blacklist across
/// requests (essential for `revoke` to actually persist between
/// /logout and the next /me with the revoked jti).
///
/// `JwtLifecycle` isn't `Clone` (its blacklist is a `RwLock`) so we
/// hand handlers a `&'static JwtLifecycle` rather than copying.
static JWT: OnceLock<JwtLifecycle> = OnceLock::new();

/// Build the JWT auth router with the given [`Config`]. Mount it
/// alongside your app's API routes — the endpoints are tenant-aware
/// via the [`Tenant`] extractor, so requests resolve against the
/// correct subdomain's user table.
///
/// Calling `jwt_router` more than once per process is a no-op for
/// the JWT lifecycle: only the first call's config is honored. This
/// is fine for the common case (mount once at boot) and prevents
/// silently-divergent signing keys across multiple mount sites.
pub fn jwt_router(cfg: Config) -> Router<()> {
    let _ = JWT.set(cfg.build_jwt());

    Router::new()
        .route(&format!("{}/login", cfg.prefix), post(login))
        .route(&format!("{}/refresh", cfg.prefix), post(refresh))
        .route(&format!("{}/logout", cfg.prefix), post(logout))
        .route(&format!("{}/me", cfg.prefix), get(me))
}

/// Share access to the singleton [`JwtLifecycle`]. Falls back to a
/// default-config build for tests or unwired setups so the panic
/// surface is "401: invalid token" rather than "deref of None".
fn jwt_handle() -> &'static JwtLifecycle {
    JWT.get_or_init(|| Config::default().build_jwt())
}

// ---------------------------------------------------------------- Handlers

#[derive(Debug, Deserialize)]
pub struct LoginInput {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct UserBrief {
    pub user_id: i64,
    pub username: String,
    pub is_superuser: bool,
}

#[derive(Debug, Serialize)]
pub struct LoginOutput {
    pub access: String,
    pub refresh: String,
    pub user: UserBrief,
}

async fn login(
    t: Tenant,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginInput>,
) -> Result<Json<LoginOutput>, Response> {
    use crate::core::Column as _;
    use crate::signals::auth::{
        meta_from_headers, send_user_logged_in, send_user_login_failed, AuthFailureReason,
        UserLoggedInContext, UserLoginFailedContext,
    };
    use crate::sql::FetcherPool as _;
    use crate::tenancy::auth::User;

    let meta = meta_from_headers(&headers, Some("/auth/login"));
    let fire_failed = |reason: AuthFailureReason| -> UserLoginFailedContext {
        UserLoginFailedContext {
            source: "jwt",
            attempted_username: Some(body.username.clone()),
            reason,
            request: meta.clone(),
        }
    };

    let users = User::objects()
        .where_(User::username.eq(body.username.clone()))
        .fetch_pool(t.pool())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())?;

    let Some(user) = users.into_iter().next() else {
        // H1: spend a verify's worth of work on the unknown-user path so
        // timing doesn't reveal whether the username exists.
        crate::tenancy::password::verify_dummy(&body.password);
        send_user_login_failed(fire_failed(AuthFailureReason::InvalidCredentials)).await;
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials").into_response());
    };

    // Audit M1 — per-account brute-force lockout, on by default. Key is
    // scoped by tenant slug + resolved user id so it can't collide with
    // another tenant's same-numbered user (or the operator/admin
    // domains). A locked account short-circuits before the verify.
    let uid = user.id.get().copied().unwrap_or(0);
    #[cfg(feature = "cache")]
    let lock_key = format!("tenant:{}:{}", t.org.slug, uid);
    #[cfg(feature = "cache")]
    if crate::account_lockout::shared().is_locked(&lock_key).await {
        send_user_login_failed(fire_failed(AuthFailureReason::InvalidCredentials)).await;
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials").into_response());
    }

    // Verify before the active check so active vs inactive accounts take
    // the same time (audit H1).
    let ok = crate::tenancy::password::verify(&body.password, &user.password_hash)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())?;

    if !user.active {
        send_user_login_failed(fire_failed(AuthFailureReason::Inactive)).await;
        // Audit M4 — at the login endpoint, an inactive account must
        // look identical to an unknown user / wrong password (same 401
        // + message) so the API can't be used to enumerate accounts.
        // The Inactive signal above still records the real reason. The
        // `me` handler keeps its 403 (the caller already proved it holds
        // a valid token for this user, so it's not an enumeration vector).
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials").into_response());
    }
    if !ok {
        // Audit M1 — count this failure toward the per-account lockout.
        #[cfg(feature = "cache")]
        {
            let _ = crate::account_lockout::shared()
                .record_failure(&lock_key)
                .await;
        }
        send_user_login_failed(fire_failed(AuthFailureReason::InvalidCredentials)).await;
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials").into_response());
    }

    // Audit M1 — successful login clears the failure counter + any lock.
    #[cfg(feature = "cache")]
    crate::account_lockout::shared().clear(&lock_key).await;

    let user_id = uid;
    send_user_logged_in(UserLoggedInContext {
        source: "jwt",
        user_id,
        username: user.username.clone(),
        is_superuser: user.is_superuser,
        request: meta,
    })
    .await;
    // Bake the resolved tenant's slug into the token so a JWT
    // signed on `acme.<apex>` cannot be replayed on
    // `sju.<apex>` — even though both tenants share
    // RUSTANGO_SESSION_SECRET. Without this binding, `sub: 1`
    // means "the user with id=1", and id=1 likely exists on
    // every tenant. With the binding, verify checks the resolved
    // request's tenant slug against the claim.
    let custom = serde_json::json!({"tenant": t.org.slug});
    let pair = jwt_handle()
        .issue_pair_with(user_id, custom.as_object().cloned().unwrap_or_default())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())?;

    Ok(Json(LoginOutput {
        access: pair.access,
        refresh: pair.refresh,
        user: UserBrief {
            user_id,
            username: user.username,
            is_superuser: user.is_superuser,
        },
    }))
}

#[derive(Debug, Deserialize)]
pub struct RefreshInput {
    pub refresh: String,
}

#[derive(Debug, Serialize)]
pub struct RefreshOutput {
    pub access: String,
    pub refresh: String,
}

/// Rotate a refresh token. The old refresh token's `jti` is revoked
/// at the framework level (`JwtLifecycle::refresh` does this), so a
/// stolen refresh token is single-use — the legitimate user's next
/// refresh succeeds and invalidates whatever the attacker also tried
/// to use.
async fn refresh(
    t: Tenant,
    Json(body): Json<RefreshInput>,
) -> Result<Json<RefreshOutput>, Response> {
    // Audit N3 — bind refresh to the resolved tenant. Without this, a
    // refresh token minted on tenant A could be POSTed to tenant B's
    // /refresh (they share the session secret) and rotated — burning A's
    // refresh token (a cross-tenant DoS / rotation oracle). Verify the
    // token's `tenant` claim matches this subdomain BEFORE rotating.
    let claims = jwt_handle().verify_refresh(&body.refresh).ok_or_else(|| {
        (StatusCode::UNAUTHORIZED, "invalid or expired refresh token").into_response()
    })?;
    let tenant_ok =
        claims.custom_value("tenant").and_then(|v| v.as_str()) == Some(t.org.slug.as_str());
    if !tenant_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            "refresh token issued for a different tenant",
        )
            .into_response());
    }
    // Audit P2 — re-check the account is still active (and exists) before
    // minting a fresh pair. Otherwise a user deactivated/deleted after
    // login could keep rotating refresh tokens for the whole refresh TTL,
    // getting a fresh access token every cycle. Same uniform 401 as other
    // refresh failures (the caller already proved token possession, so
    // this isn't an enumeration vector, but uniformity leaks nothing).
    {
        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        use crate::tenancy::auth::User;
        let users: Vec<User> = User::objects()
            .where_(User::id.eq(claims.sub))
            .fetch_pool(t.pool())
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())?;
        let still_active = users.into_iter().next().is_some_and(|u| u.active);
        if !still_active {
            return Err(
                (StatusCode::UNAUTHORIZED, "invalid or expired refresh token").into_response(),
            );
        }
    }
    let pair = jwt_handle().refresh(&body.refresh).ok_or_else(|| {
        (StatusCode::UNAUTHORIZED, "invalid or expired refresh token").into_response()
    })?;
    Ok(Json(RefreshOutput {
        access: pair.access,
        refresh: pair.refresh,
    }))
}

/// Revoke the access token's `jti`. Subsequent requests with the
/// same token return 401 even though `exp` would otherwise still be
/// valid.
async fn logout(
    t: Tenant,
    headers: axum::http::HeaderMap,
    bearer: Bearer,
) -> Result<StatusCode, Response> {
    use crate::signals::auth::{meta_from_headers, send_user_logged_out, UserLoggedOutContext};
    // Best-effort: decode the bearer to recover the user id for the
    // signal. We don't reject on verify-failure here — the revoke
    // call below still runs, and a stale/expired token logout is a
    // valid audit event in its own right.
    let claims = jwt_handle().verify_access(&bearer.0);
    // Audit N3 — if the token IS valid but bound to a DIFFERENT tenant,
    // don't let this subdomain's endpoint revoke it. (An unverifiable /
    // expired token falls through to a best-effort revoke as before.)
    if let Some(c) = &claims {
        if c.custom_value("tenant").and_then(|v| v.as_str()) != Some(t.org.slug.as_str()) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "token issued for a different tenant",
            )
                .into_response());
        }
    }
    let user_id = claims.map(|c| c.sub);
    let meta = meta_from_headers(&headers, Some("/auth/logout"));

    jwt_handle().revoke(&bearer.0);

    send_user_logged_out(UserLoggedOutContext {
        source: "jwt",
        user_id,
        username: None,
        request: meta,
    })
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Verify a Bearer token's signature, expiry, AND tenant binding.
/// Returns the user-id from `sub` if and only if the token's
/// `tenant` claim matches the resolved request tenant — preventing
/// cross-tenant replay where a JWT minted on `acme` is sent to
/// `sju` (both tenants share the session secret, so signature alone
/// would validate). Tokens without a `tenant` claim are rejected
/// with 401 — they must have been minted by an old or external
/// issuer that doesn't honor this contract.
pub fn verify_for_tenant(bearer: &str, expected_slug: &str) -> Result<i64, &'static str> {
    let claims = jwt_handle()
        .verify_access(bearer)
        .ok_or("invalid or expired token")?;
    let claim_tenant = claims
        .custom_value("tenant")
        .and_then(|v| v.as_str())
        .ok_or("token missing tenant binding")?;
    if claim_tenant != expected_slug {
        return Err("token issued for different tenant");
    }
    Ok(claims.sub)
}

/// Return identity info for the user named in the access token's
/// `sub` claim. Hits the tenant DB for the authoritative
/// `is_superuser` and `active` flags — useful when the client wants
/// to render UI based on current state without re-querying every
/// app endpoint.
///
/// Validates the token's `tenant` claim against the resolved
/// request tenant via [`verify_for_tenant`] so a JWT minted on one
/// subdomain can't be replayed against another.
async fn me(t: Tenant, bearer: Bearer) -> Result<Json<UserBrief>, Response> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    use crate::tenancy::auth::User;

    let user_id = verify_for_tenant(&bearer.0, &t.org.slug)
        .map_err(|msg| (StatusCode::UNAUTHORIZED, msg).into_response())?;

    let users = User::objects()
        .where_(User::id.eq(user_id))
        .fetch_pool(t.pool())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())?;

    let user = users
        .into_iter()
        .next()
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "user not found").into_response())?;

    if !user.active {
        return Err((StatusCode::FORBIDDEN, "account inactive").into_response());
    }

    Ok(Json(UserBrief {
        user_id: user.id.get().copied().unwrap_or(0),
        username: user.username,
        is_superuser: user.is_superuser,
    }))
}

// ---------------------------------------------------------------- Bearer

/// Extracts the raw bearer token from the `Authorization` header.
/// Rejects with 401 when missing or malformed. Distinct from
/// [`crate::tenancy::auth_backends::JwtBackend`] — that backend
/// looks the user row up against a `PgPool` (single-tenant); this
/// extractor just pulls the token string and lets handlers do the
/// per-tenant lookup themselves via [`Tenant`].
struct Bearer(String);

impl<S: Send + Sync> FromRequestParts<S> for Bearer {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(|t| Bearer(t.trim().to_owned()))
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, "missing Bearer token").into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_paths_match_documentation() {
        let cfg = Config::default();
        assert_eq!(cfg.prefix, "/api/auth");
        assert_eq!(cfg.access_ttl_secs, 900);
        assert_eq!(cfg.refresh_ttl_secs, 7 * 86400);
        assert!(cfg.session_secret.is_none());
    }

    #[test]
    #[should_panic(expected = "JWT signing key")]
    fn build_jwt_panics_on_empty_secret() {
        // Audit C1: an empty key is publicly guessable and would let
        // anyone forge tokens. build_jwt must fail closed. Use an
        // explicit empty session_secret (not the env var) so the test
        // is deterministic regardless of the ambient environment.
        let cfg = Config {
            session_secret: Some(Vec::new()),
            ..Default::default()
        };
        let _ = cfg.build_jwt();
    }

    #[test]
    #[should_panic(expected = "JWT signing key")]
    fn build_jwt_panics_on_short_secret() {
        // A sub-32-byte key is rejected too — not just the empty case.
        let cfg = Config {
            session_secret: Some(b"too-short-key".to_vec()),
            ..Default::default()
        };
        let _ = cfg.build_jwt();
    }

    #[test]
    fn config_uses_explicit_secret_when_set() {
        let cfg = Config {
            session_secret: Some(b"super-secret-key-for-tests-32b!!".to_vec()),
            ..Default::default()
        };
        assert!(cfg.session_secret.as_ref().unwrap().len() >= 32);
        let jwt = cfg.build_jwt();
        let token = jwt.issue_pair(42);
        let claims = jwt.verify_access(&token.access).expect("access valid");
        assert_eq!(claims.sub, 42);
    }

    #[test]
    fn jwt_router_mounts_all_four_endpoints() {
        // Pass a valid 32-byte secret — build_jwt now fails closed on a
        // short/empty key, so the smoke test must supply a real one.
        let cfg = Config {
            session_secret: Some(b"router-smoke-test-secret-32-byte".to_vec()),
            ..Default::default()
        };
        let r = jwt_router(cfg);
        // Smoke: building the router doesn't panic. The actual route
        // registration is exercised via integration tests that send
        // requests against the constructed router.
        let _ = r;
    }

    /// `Config::with_jwt_settings` honors TOML access/refresh TTLs
    /// when set; falls through to the Config default when None (#87).
    #[cfg(feature = "config")]
    #[test]
    fn with_jwt_settings_overrides_ttls() {
        let mut s = crate::config::JwtSettings::default();
        s.access_ttl_secs = Some(60); // 1 min
        s.refresh_ttl_secs = Some(3600); // 1h
        let cfg = Config::default().with_jwt_settings(&s);
        assert_eq!(cfg.access_ttl_secs, 60);
        assert_eq!(cfg.refresh_ttl_secs, 3600);
    }

    /// Missing TOML keys preserve the Config defaults — partial
    /// config files don't reset unspecified fields.
    #[cfg(feature = "config")]
    #[test]
    fn with_jwt_settings_unset_preserves_defaults() {
        let s = crate::config::JwtSettings::default(); // both fields None
        let cfg = Config::default().with_jwt_settings(&s);
        assert_eq!(cfg.access_ttl_secs, 900); // 15 min
        assert_eq!(cfg.refresh_ttl_secs, 7 * 86400); // 7 days
    }
}
