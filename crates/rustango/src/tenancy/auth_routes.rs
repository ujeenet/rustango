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
//! use rustango::tenancy::auth_routes::{Config, JwtAuth};
//!
//! let auth = JwtAuth::new(Config::default());
//! rustango::manage::Cli::new()
//!     .tenancy()
//!     .api(my_app::urls::api().merge(auth.router()))
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

use std::sync::Arc;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::extractors::Tenant;
use crate::sql::FetcherPool as _;
use crate::tenancy::jwt_lifecycle::JwtLifecycle;

// ---------------------------------------------------------------- Config

/// Knobs for [`JwtAuth`]. All have sensible defaults; override
/// when integrating with non-default URL prefixes (#74), shorter
/// access TTLs, custom signing keys, etc.
#[derive(Clone)]
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
    /// Revocation store. `None` keeps the default
    /// [`InMemoryJtiStore`](crate::jti_store::InMemoryJtiStore), which
    /// is single-process and forgets every revocation on restart — so
    /// on more than one replica `/logout` is best-effort (#1190). Pass
    /// a Redis- or database-backed store for a real deployment.
    pub jti_store: Option<Arc<dyn crate::jti_store::JtiStore>>,
    /// Extra claims baked into both tokens at login, on top of the
    /// `tenant` claim the router always sets (#1190).
    ///
    /// Returning a reserved name (`sub`, `exp`, `jti`, `typ`) fails the
    /// login with a 500 rather than silently dropping it; `tenant` is
    /// the router's own and is not overridable either.
    pub extra_claims: Option<ClaimsHook>,
}

/// Builds the per-login custom claims for [`Config::extra_claims`].
pub type ClaimsHook =
    Arc<dyn Fn(&ClaimsContext<'_>) -> serde_json::Map<String, serde_json::Value> + Send + Sync>;

/// What the router knows about the user it just authenticated, handed
/// to [`Config::extra_claims`].
#[derive(Debug)]
#[non_exhaustive]
pub struct ClaimsContext<'a> {
    pub user_id: i64,
    pub username: &'a str,
    pub is_superuser: bool,
    /// Slug of the tenant the request resolved to.
    pub tenant_slug: &'a str,
}

// Hand-written: neither a `dyn JtiStore` nor a boxed closure is
// `Debug`, and `Config` is in enough public signatures to want it.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("prefix", &self.prefix)
            .field("access_ttl_secs", &self.access_ttl_secs)
            .field("refresh_ttl_secs", &self.refresh_ttl_secs)
            // Never the key itself — only whether one was set.
            .field("session_secret", &self.session_secret.is_some())
            .field("jti_store", &self.jti_store.is_some())
            .field("extra_claims", &self.extra_claims.is_some())
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: "/api/auth".to_owned(),
            access_ttl_secs: 900,
            refresh_ttl_secs: 7 * 86400,
            session_secret: None,
            jti_store: None,
            extra_claims: None,
        }
    }
}

/// Minimum HMAC key length (bytes) accepted for JWT signing. 32 bytes
/// matches the SHA-256 block/output size and `SessionSecret`'s own
/// floor. Shorter keys — in particular the **empty** key produced when
/// `RUSTANGO_SESSION_SECRET` is unset and no explicit `session_secret`
/// is given — are publicly guessable and would let anyone forge tokens
/// (the 2026-06 authentication audit, finding C1).
const MIN_HMAC_KEY_LEN: usize = 32;

impl Config {
    fn build_jwt(&self) -> JwtLifecycle {
        // `RUSTANGO_SESSION_SECRET` is base64, and this used to take the
        // raw string bytes (#1396). A 32-character base64 secret is 24
        // bytes of key — it cleared the 32-byte floor below while the
        // cookie layer, which decodes, rejected the same value. One
        // variable, two keys, and the assert measuring the wrong thing.
        let secret = match &self.session_secret {
            Some(explicit) => explicit.clone(),
            None => {
                let raw = std::env::var("RUSTANGO_SESSION_SECRET").unwrap_or_default();
                crate::session::SessionSecret::from_b64(&raw)
                    .map(|s| s.key().to_vec())
                    .unwrap_or_default()
            }
        };
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
        let jwt = JwtLifecycle::new(secret)
            .with_access_ttl(self.access_ttl_secs)
            .with_refresh_ttl(self.refresh_ttl_secs);
        match &self.jti_store {
            Some(store) => jwt.with_jti_store(Arc::clone(store)),
            None => jwt,
        }
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
    /// api.merge(auth_routes::JwtAuth::new(auth).router())
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

/// One configured JWT auth: the signing key, revocation store and claims
/// hook, shared by the router, [`require_bearer`] and
/// [`JwtAuth::verify_for_tenant`]. Cheap to clone.
///
/// Build **one** and share it: a second instance has its own in-memory
/// revocation list, so a logout through one is not seen by the other.
///
/// ```no_run
/// use axum::{middleware, Router};
/// use rustango::tenancy::auth_routes::{require_bearer, Config, JwtAuth};
///
/// # fn my_api() -> Router { Router::new() }
/// let auth = JwtAuth::new(Config {
///     session_secret: Some(vec![7; 32]),
///     ..Config::default()
/// });
/// let api = my_api()
///     .layer(middleware::from_fn_with_state(auth.clone(), require_bearer))
///     .merge(auth.router());
/// ```
#[derive(Clone)]
pub struct JwtAuth(Arc<AuthState>);

struct AuthState {
    jwt: JwtLifecycle,
    extra_claims: Option<ClaimsHook>,
    prefix: String,
}

impl JwtAuth {
    /// Build from `cfg`. Panics on a signing key under 32 bytes, so a
    /// misconfigured deployment refuses to start.
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        Self(Arc::new(AuthState {
            jwt: cfg.build_jwt(),
            extra_claims: cfg.extra_claims,
            prefix: cfg.prefix,
        }))
    }

    /// The login / refresh / logout / me endpoints under `Config::prefix`.
    /// They are tenant-aware via the [`Tenant`] extractor.
    #[must_use]
    pub fn router(&self) -> Router<()> {
        let p = &self.0.prefix;
        Router::new()
            .route(&format!("{p}/login"), post(login))
            .route(&format!("{p}/refresh"), post(refresh))
            .route(&format!("{p}/logout"), post(logout))
            .route(&format!("{p}/me"), get(me))
            .with_state(self.clone())
    }

    /// The lifecycle behind this auth, e.g. to mint a token in a test.
    #[must_use]
    pub fn lifecycle(&self) -> &JwtLifecycle {
        &self.0.jwt
    }

    /// Verify a Bearer token's signature, expiry, revocation AND tenant
    /// binding, returning `sub`.
    ///
    /// All tenants share one signing key, so the `tenant` claim is what
    /// stops a token minted on `acme` being replayed on `sju`. A token
    /// without that claim is refused.
    pub async fn verify_for_tenant(
        &self,
        bearer: &str,
        expected_slug: &str,
    ) -> Result<i64, &'static str> {
        let claims = self
            .0
            .jwt
            .verify_access(bearer)
            .await
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
}

/// Claims for a freshly issued login pair: the app's hook first, then
/// the router's own `tenant`.
///
/// The order is the point. `tenant` is what stops a token signed on
/// one subdomain being replayed on another, so a hook must not be able
/// to overwrite it — and a hook that tries is a mistake worth ignoring
/// rather than a request worth honouring (#1190).
fn login_claims(
    hook: Option<&ClaimsHook>,
    ctx: &ClaimsContext<'_>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut custom = hook.map_or_else(serde_json::Map::new, |h| h(ctx));
    custom.insert(
        "tenant".to_owned(),
        serde_json::Value::String(ctx.tenant_slug.to_owned()),
    );
    custom
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
    State(auth): State<JwtAuth>,
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
        .fetch(t.pool())
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let Some(user) = users.into_iter().next() else {
        // H1: spend a verify's worth of work on the unknown-user path so
        // timing doesn't reveal whether the username exists.
        crate::tenancy::password::verify_dummy(&body.password);
        send_user_login_failed(fire_failed(AuthFailureReason::InvalidCredentials)).await;
        return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
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
        return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
    }

    // Verify before the active check so active vs inactive accounts take
    // the same time (audit H1).
    let ok = crate::tenancy::password::verify(&body.password, &user.password_hash)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    if !user.active {
        send_user_login_failed(fire_failed(AuthFailureReason::Inactive)).await;
        // Audit M4 — at the login endpoint, an inactive account must
        // look identical to an unknown user / wrong password (same 401
        // + message) so the API can't be used to enumerate accounts.
        // The Inactive signal above still records the real reason. The
        // `me` handler keeps its 403 (the caller already proved it holds
        // a valid token for this user, so it's not an enumeration vector).
        return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
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
        return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
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
    let custom = login_claims(
        auth.0.extra_claims.as_ref(),
        &ClaimsContext {
            user_id,
            username: &user.username,
            is_superuser: user.is_superuser,
            tenant_slug: &t.org.slug,
        },
    );
    let pair = auth
        .lifecycle()
        .issue_pair_with(user_id, custom)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

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
    State(auth): State<JwtAuth>,
    t: Tenant,
    Json(body): Json<RefreshInput>,
) -> Result<Json<RefreshOutput>, Response> {
    let jwt = auth.lifecycle();
    // Audit N3 — bind refresh to the resolved tenant. Without this, a
    // refresh token minted on tenant A could be POSTed to tenant B's
    // /refresh (they share the session secret) and rotated — burning A's
    // refresh token (a cross-tenant DoS / rotation oracle). Verify the
    // token's `tenant` claim matches this subdomain BEFORE rotating.
    let claims = jwt
        .verify_refresh(&body.refresh)
        .await
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "invalid or expired refresh token"))?;
    let tenant_ok =
        claims.custom_value("tenant").and_then(|v| v.as_str()) == Some(t.org.slug.as_str());
    if !tenant_ok {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "refresh token issued for a different tenant",
        ));
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
            .fetch(t.pool())
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let still_active = users.into_iter().next().is_some_and(|u| u.active);
        if !still_active {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                "invalid or expired refresh token",
            ));
        }
    }
    let pair = jwt
        .refresh(&body.refresh)
        .await
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "invalid or expired refresh token"))?;
    Ok(Json(RefreshOutput {
        access: pair.access,
        refresh: pair.refresh,
    }))
}

/// Revoke the session — the access token's `jti`, and the refresh
/// token's when the client sends it.
///
/// The refresh half matters more than it looks (#1402). Logging out used
/// to revoke only the bearer, so the refresh token survived with its full
/// seven-day life and could mint fresh access tokens indefinitely — on a
/// credential the endpoint never looked at. A user who clicks log out on
/// a borrowed device means *this session is over*, not "one of its two
/// tokens is".
///
/// The body is optional so an existing client that sends none keeps
/// working; it simply revokes less, exactly as before. Clients should
/// send `{"refresh": "…"}`.
#[derive(Debug, Deserialize)]
pub struct LogoutInput {
    /// The refresh token to revoke alongside the bearer. Optional for
    /// back-compat with clients written against the old endpoint.
    #[serde(default)]
    pub refresh: Option<String>,
}

async fn logout(
    State(auth): State<JwtAuth>,
    t: Tenant,
    headers: axum::http::HeaderMap,
    bearer: Bearer,
    body: Option<Json<LogoutInput>>,
) -> Result<StatusCode, Response> {
    let jwt = auth.lifecycle();
    use crate::signals::auth::{meta_from_headers, send_user_logged_out, UserLoggedOutContext};
    // Best-effort: decode the bearer to recover the user id for the
    // signal. We don't reject on verify-failure here — the revoke
    // call below still runs, and a stale/expired token logout is a
    // valid audit event in its own right.
    let claims = jwt.verify_access(&bearer.0).await;
    // Audit N3 — if the token IS valid but bound to a DIFFERENT tenant,
    // don't let this subdomain's endpoint revoke it. (An unverifiable /
    // expired token falls through to a best-effort revoke as before.)
    if let Some(c) = &claims {
        if c.custom_value("tenant").and_then(|v| v.as_str()) != Some(t.org.slug.as_str()) {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                "token issued for a different tenant",
            ));
        }
    }
    let user_id = claims.map(|c| c.sub);
    let meta = meta_from_headers(&headers, Some("/auth/logout"));

    jwt.revoke(&bearer.0).await;

    // The refresh token too, when we were given one. Revoking it is what
    // actually ends the session: the access token expires on its own in
    // minutes, the refresh token would have outlived the logout by days
    // and could mint replacements the whole time.
    //
    // Tenant-pinned the same way the bearer is, so one subdomain cannot
    // revoke another tenant's token by posting it here.
    if let Some(Json(input)) = body {
        if let Some(refresh) = input.refresh.as_deref() {
            let ok = match jwt.verify_refresh(refresh).await {
                Some(c) => {
                    c.custom_value("tenant").and_then(|v| v.as_str()) == Some(t.org.slug.as_str())
                }
                // Unverifiable or expired: revoke best-effort, matching
                // how the bearer is treated above. A stale token being
                // logged out is still a valid thing to record.
                None => true,
            };
            if ok {
                jwt.revoke(refresh).await;
            }
        }
    }

    send_user_logged_out(UserLoggedOutContext {
        source: "jwt",
        user_id,
        username: None,
        request: meta,
    })
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Return identity info for the user named in the access token's
/// `sub` claim. Hits the tenant DB for the authoritative
/// `is_superuser` and `active` flags — useful when the client wants
/// to render UI based on current state without re-querying every
/// app endpoint.
///
/// Validates the token's `tenant` claim against the resolved
/// request tenant via [`JwtAuth::verify_for_tenant`] so a JWT minted on
/// one subdomain can't be replayed against another.
async fn me(
    State(auth): State<JwtAuth>,
    t: Tenant,
    bearer: Bearer,
) -> Result<Json<UserBrief>, Response> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    use crate::tenancy::auth::User;

    let user_id = auth
        .verify_for_tenant(&bearer.0, &t.org.slug)
        .await
        .map_err(|msg| err(StatusCode::UNAUTHORIZED, msg))?;

    let users = User::objects()
        .where_(User::id.eq(user_id))
        .fetch(t.pool())
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let user = users
        .into_iter()
        .next()
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "user not found"))?;

    if !user.active {
        return Err(err(StatusCode::FORBIDDEN, "account inactive"));
    }

    Ok(Json(UserBrief {
        user_id: user.id.get().copied().unwrap_or(0),
        username: user.username,
        is_superuser: user.is_superuser,
    }))
}

// ------------------------------------------------- Bearer → Principal layer

/// Require a valid, tenant-pinned access token, and publish who sent it.
///
/// This is the API counterpart of the session-cookie middleware: it turns
/// `Authorization: Bearer …` into an [`AuthenticatedUser`] and a [`Principal`]
/// in the request extensions, which is what every downstream consumer reads —
/// `ViewSet` permission gates, [`OwnedBy`] scoping, handlers taking the
/// `Principal` extractor.
///
/// What it checks, in order:
/// 1. signature, expiry, `typ = access`, and the JTI revocation list, via
///    [`JwtAuth::verify_for_tenant`];
/// 2. the token's `tenant` claim against the resolved tenant — all tenants
///    share one signing key, so this is the only thing standing between a
///    token minted on `acme.` and a request to `globex.`;
/// 3. the user row, **read per request**, so deactivating an account takes
///    effect immediately rather than whenever the access token happens to
///    expire.
///
/// ```no_run
/// use axum::{middleware, Router};
/// use rustango::tenancy::auth_routes::{require_bearer, Config, JwtAuth};
///
/// # fn api() -> Router { Router::new() }
/// let auth = JwtAuth::new(Config {
///     session_secret: Some(vec![7; 32]),
///     ..Config::default()
/// });
/// let protected = api().layer(middleware::from_fn_with_state(auth, require_bearer));
/// ```
///
/// Mount it on the routes that need a user, not on `/auth/login` or
/// `/auth/refresh` — those are how a client gets a token in the first place.
///
/// [`AuthenticatedUser`]: crate::tenancy::AuthenticatedUser
/// [`Principal`]: crate::tenancy::Principal
/// [`OwnedBy`]: crate::viewset::OwnedBy
pub async fn require_bearer(
    State(auth): State<JwtAuth>,
    t: Tenant,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(token) = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return unauthorized("missing Bearer token");
    };

    let user_id = match auth.verify_for_tenant(token, &t.org.slug).await {
        Ok(id) => id,
        // The reason is deliberately not echoed: "expired" vs "wrong tenant"
        // vs "revoked" tells a prober which of those they achieved.
        Err(_) => return unauthorized("invalid or expired token"),
    };

    let user = match crate::tenancy::auth::User::objects()
        .filter("id", user_id)
        .fetch(t.pool())
        .await
    {
        Ok(rows) => rows.into_iter().next(),
        Err(e) => {
            tracing::error!(error = %e, "bearer auth could not read the user row");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "authentication failed");
        }
    };
    let Some(user) = user.filter(|u| u.active) else {
        // Deleted or deactivated between mint and use. Same body as a bad
        // token: whether an account exists is not a fact this endpoint owes.
        return unauthorized("invalid or expired token");
    };

    let id = user.id.get().copied().unwrap_or(user_id);
    req.extensions_mut()
        .insert(crate::tenancy::AuthenticatedUser {
            id,
            username: user.username.clone(),
            is_superuser: user.is_superuser,
        });
    req.extensions_mut().insert(crate::tenancy::Principal::user(
        id,
        user.is_superuser,
        Some(t.org.slug.clone()),
    ));
    next.run(req).await
}

fn unauthorized(msg: &'static str) -> Response {
    err(StatusCode::UNAUTHORIZED, msg)
}

/// An `ApiError` body; a 5xx cause is logged, not sent (#1684).
fn err(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    crate::api_errors::ApiError::logged(status, "auth_routes", msg).into_response()
}

// ---------------------------------------------------------------- Bearer

/// Extracts the raw bearer token from the `Authorization` header.
/// Rejects with 401 when missing or malformed. Distinct from
/// [`crate::tenancy::auth_backends::JwtBackend`] — that backend
/// looks the user row up against a `PgPool` (single-tenant); this
/// extractor just pulls the token string and lets handlers do the
/// per-tenant lookup themselves via [`Tenant`].
pub struct Bearer(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Bearer {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(|t| Bearer(t.trim().to_owned()))
            .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing Bearer token"))
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

    /// The claims hook reaches the token, and the router's `tenant`
    /// binding survives a hook that tries to replace it (#1190).
    #[test]
    fn the_claims_hook_adds_claims_but_cannot_rewrite_tenant() {
        let ctx = ClaimsContext {
            user_id: 7,
            username: "alice",
            is_superuser: false,
            tenant_slug: "acme",
        };

        // No hook: the tenant binding alone.
        let plain = login_claims(None, &ctx);
        assert_eq!(plain.get("tenant").and_then(|v| v.as_str()), Some("acme"));
        assert_eq!(plain.len(), 1);

        // A hook adding its own claims — the case the issue was filed
        // for (`fam` is a token family, for replay detection).
        let hook: ClaimsHook = Arc::new(|c: &ClaimsContext<'_>| {
            let mut m = serde_json::Map::new();
            m.insert("fam".into(), serde_json::Value::String("f-1".into()));
            m.insert("su".into(), serde_json::Value::Bool(c.is_superuser));
            m.insert(
                "who".into(),
                serde_json::Value::String(c.username.to_owned()),
            );
            m
        });
        let out = login_claims(Some(&hook), &ctx);
        assert_eq!(out.get("fam").and_then(|v| v.as_str()), Some("f-1"));
        assert_eq!(
            out.get("su").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(out.get("who").and_then(|v| v.as_str()), Some("alice"));
        assert_eq!(out.get("tenant").and_then(|v| v.as_str()), Some("acme"));

        // A hook that tries to forge a different tenant loses.
        let evil: ClaimsHook = Arc::new(|_: &ClaimsContext<'_>| {
            let mut m = serde_json::Map::new();
            m.insert("tenant".into(), serde_json::Value::String("victim".into()));
            m
        });
        let out = login_claims(Some(&evil), &ctx);
        assert_eq!(
            out.get("tenant").and_then(|v| v.as_str()),
            Some("acme"),
            "the router's tenant binding must win — it is what stops \
             cross-subdomain replay"
        );
    }

    /// A custom store reaches the lifecycle the router hands its
    /// handlers, rather than being dropped on the floor (#1190).
    #[tokio::test]
    async fn a_custom_jti_store_is_installed() {
        use crate::jti_store::{JtiFuture, JtiStore};

        #[derive(Default)]
        struct MarkerStore(std::sync::atomic::AtomicUsize);
        impl JtiStore for MarkerStore {
            fn is_used<'a>(&'a self, _jti: &'a str) -> JtiFuture<'a, bool> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async { false })
            }
            fn mark_used<'a>(&'a self, _jti: &'a str, _exp_unix: i64) -> JtiFuture<'a, bool> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async { true })
            }
        }

        let store = Arc::new(MarkerStore::default());
        let cfg = Config {
            session_secret: Some(b"a-test-signing-key-of-32-bytes!!".to_vec()),
            jti_store: Some(store.clone()),
            ..Config::default()
        };
        let jwt = cfg.build_jwt();
        let pair = jwt.issue_pair(1);

        // Revoking goes through the store we installed. Against the
        // default in-memory store this counter stays at 0.
        jwt.revoke(&pair.access).await;
        assert!(
            store.0.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "the configured JtiStore must be the one the lifecycle uses"
        );
    }

    /// Two configs stay two. Under the old `OnceLock` the first one's key
    /// signed for both, so B accepted A's token (#1190).
    #[tokio::test]
    async fn each_jwt_auth_keeps_its_own_key() {
        let auth = |key: &[u8]| {
            JwtAuth::new(Config {
                session_secret: Some(key.to_vec()),
                ..Config::default()
            })
        };
        let a = auth(b"key-a-key-a-key-a-key-a-key-a-32");
        let b = auth(b"key-b-key-b-key-b-key-b-key-b-32");
        let mut custom = serde_json::Map::new();
        custom.insert("tenant".into(), "acme".into());
        let token = a.lifecycle().issue_pair_with(1, custom).unwrap().access;

        assert_eq!(a.verify_for_tenant(&token, "acme").await, Ok(1));
        assert!(b.verify_for_tenant(&token, "acme").await.is_err());
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

    #[tokio::test]
    async fn config_uses_explicit_secret_when_set() {
        let cfg = Config {
            session_secret: Some(b"super-secret-key-for-tests-32b!!".to_vec()),
            ..Default::default()
        };
        assert!(cfg.session_secret.as_ref().unwrap().len() >= 32);
        let jwt = cfg.build_jwt();
        let token = jwt.issue_pair(42);
        let claims = jwt
            .verify_access(&token.access)
            .await
            .expect("access valid");
        assert_eq!(claims.sub, 42);
    }

    #[test]
    fn router_mounts_all_four_endpoints() {
        // Pass a valid 32-byte secret — build_jwt now fails closed on a
        // short/empty key, so the smoke test must supply a real one.
        let cfg = Config {
            session_secret: Some(b"router-smoke-test-secret-32-byte".to_vec()),
            ..Default::default()
        };
        let r = JwtAuth::new(cfg).router();
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
