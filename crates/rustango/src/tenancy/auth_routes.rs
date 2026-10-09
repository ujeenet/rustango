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

use crate::extractors::{Tenant, TenantScope};
use crate::sql::sqlx::Database;
use crate::sql::FetcherPool as _;
use crate::tenancy::jwt_lifecycle::{JwtLifecycle, UserTokenScope};
use crate::tenancy::DefaultTenantDb;

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
    /// Longest a login lasts across refreshes, in seconds. Default 30 days (#1854).
    pub refresh_absolute_ttl_secs: i64,
    /// A rotated refresh token sent again within about this many seconds
    /// (up to twice it) is a client retry: 401 without revoking the chain.
    /// Default 10; `0` treats every reuse as theft (#1854).
    pub refresh_reuse_grace_secs: i64,
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
    /// login with a 500 rather than silently dropping it, and so does
    /// `kind`; `tenant`, `pwf`, `sat` and `fam` are the router's own.
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
            .field("refresh_absolute_ttl_secs", &self.refresh_absolute_ttl_secs)
            .field("refresh_reuse_grace_secs", &self.refresh_reuse_grace_secs)
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
            refresh_absolute_ttl_secs: 30 * 86400,
            refresh_reuse_grace_secs: 10,
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
    #[cfg(test)]
    fn build_jwt(&self) -> JwtLifecycle {
        self.build_jwt_with(self.signing_key())
    }

    fn signing_key(&self) -> Vec<u8> {
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
        secret
    }

    fn build_jwt_with(&self, secret: Vec<u8>) -> JwtLifecycle {
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
    /// Same key as `jwt`; fingerprints the password hash into `pwf`.
    pwf_secret: crate::session::SessionSecret,
    session_cap_secs: i64,
    reuse_grace_secs: i64,
    extra_claims: Option<ClaimsHook>,
    prefix: String,
}

impl JwtAuth {
    /// Build from `cfg`. Panics on a signing key under 32 bytes, so a
    /// misconfigured deployment refuses to start.
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        assert!(
            cfg.refresh_absolute_ttl_secs > 0,
            "auth_routes::Config::refresh_absolute_ttl_secs must be > 0; every refresh would fail",
        );
        assert!(
            cfg.refresh_reuse_grace_secs >= 0,
            "auth_routes::Config::refresh_reuse_grace_secs must be >= 0",
        );
        let key = cfg.signing_key();
        let pwf_secret = crate::session::SessionSecret::from_bytes(key.clone());
        let jwt = cfg.build_jwt_with(key);
        if jwt.jti_store_is_process_local() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    target: "rustango::tenancy",
                    "JWT revocation uses an in-memory store: a logged-out token still works on \
                     other replicas and after a restart; set `auth_routes::Config::jti_store` \
                     to a Redis or database store"
                );
            });
        }
        Self(Arc::new(AuthState {
            jwt,
            pwf_secret,
            session_cap_secs: cfg.refresh_absolute_ttl_secs,
            reuse_grace_secs: cfg.refresh_reuse_grace_secs,
            extra_claims: cfg.extra_claims,
            prefix: cfg.prefix,
        }))
    }

    /// The login / refresh / logout / me endpoints under `Config::prefix`.
    /// They are tenant-aware via the [`Tenant`] extractor.
    #[must_use]
    pub fn router(&self) -> Router<()> {
        self.router_for::<DefaultTenantDb>()
    }

    /// [`Self::router`] for a `Tenant<DB>` other than the default, e.g.
    /// SQLite in a build that also enables `postgres` (#1778).
    #[must_use]
    pub fn router_for<DB>(&self) -> Router<()>
    where
        DB: Database,
        Tenant<DB>: FromRequestParts<JwtAuth> + Send,
    {
        let p = &self.0.prefix;
        Router::new()
            .route(&format!("{p}/login"), post(login::<DB>))
            .route(&format!("{p}/refresh"), post(refresh::<DB>))
            .route(&format!("{p}/logout"), post(logout::<DB>))
            .route(&format!("{p}/me"), get(me::<DB>))
            .with_state(self.clone())
    }

    /// The lifecycle behind this auth, e.g. to mint a token in a test.
    #[must_use]
    pub fn lifecycle(&self) -> &JwtLifecycle {
        &self.0.jwt
    }

    /// Verify a Bearer token's signature, expiry, revocation AND tenant
    /// binding, then that its user is active and its session alive,
    /// returning `sub` — the same checks as [`require_bearer`].
    ///
    /// All tenants share one signing key, so the `tenant` claim is what
    /// stops a token minted on `acme` being replayed on `sju`. A token
    /// without that claim is refused, and so is an MCP agent token. The
    /// user row is read from `tenant`'s pool, so a logout or password
    /// change ends the token at once (#2118).
    /// Takes what it needs from `tenant` up front, so the future is `Send`
    /// for any `DB`, as in #1778.
    pub fn verify_for_tenant<DB: Database>(
        &self,
        bearer: &str,
        tenant: &Tenant<DB>,
    ) -> impl std::future::Future<Output = Result<i64, &'static str>> + Send + 'static {
        let (auth, bearer) = (self.clone(), bearer.to_owned());
        let (slug, pool) = (tenant.org.slug.clone(), tenant.pool().clone());
        async move {
            const REFUSED: &str = "invalid or expired token";
            match session_user_in(&auth, &bearer, &slug, &pool).await {
                Ok(Some(u)) if u.active => u.id.get().copied().ok_or(REFUSED),
                Ok(_) | Err(SessionLookup::Refused) => Err(REFUSED),
                Err(SessionLookup::Db(e)) => {
                    tracing::error!(target: "rustango::tenancy", error = %e, "verify_for_tenant: user lookup");
                    Err("user lookup failed")
                }
            }
        }
    }
}

/// Compile-time: `verify_for_tenant` is `Send` for a generic `DB` (#1778).
#[allow(dead_code)]
fn verify_for_tenant_is_send<DB: Database>(auth: &JwtAuth, t: &Tenant<DB>) {
    fn send<T: Send>(_: T) {}
    send(auth.verify_for_tenant("", t));
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

/// Router-owned claims that tie a refresh chain to one login (#1854).
/// `jwt.refresh` copies custom claims, so every rotation carries them.
pub(crate) struct RefreshSession {
    /// Fingerprint of the password hash at login.
    pwf: crate::session::PasswordFingerprint,
    /// Session start: the login's `iat`, for the absolute cap.
    sat: i64,
    /// Family id, revoked when a rotated token is replayed.
    fam: String,
}

/// What a bearer path checks a login session against (#2247).
pub(crate) struct SessionCheck<'a> {
    pub(crate) pwf_secret: &'a crate::session::SessionSecret,
    /// Where revoked families are recorded; `None` skips that check.
    pub(crate) families: Option<&'a dyn crate::jti_store::JtiStore>,
    /// The absolute cap; `None` where the backend does not know it.
    pub(crate) cap_secs: Option<i64>,
}

impl RefreshSession {
    const PWF: &'static str = "pwf";
    const SAT: &'static str = "sat";
    const FAM: &'static str = "fam";

    fn start(auth: &JwtAuth, user: &crate::tenancy::auth::User) -> Self {
        Self {
            pwf: crate::session::PasswordFingerprint::of(&auth.0.pwf_secret, &user.password_hash),
            // After the last logout, so a login in that second still refreshes (#2036).
            sat: crate::session::issued_at(user.sessions_revoked_at),
            fam: crate::tenancy::jwt_lifecycle::random_jti(),
        }
    }

    fn write(&self, claims: &mut serde_json::Map<String, serde_json::Value>) {
        let pwf = serde_json::to_value(&self.pwf).unwrap_or_default();
        claims.insert(Self::PWF.to_owned(), pwf);
        claims.insert(Self::SAT.to_owned(), self.sat.into());
        claims.insert(Self::FAM.to_owned(), self.fam.clone().into());
    }

    /// `None` for a token minted before these claims existed.
    pub(crate) fn read(claims: &serde_json::Map<String, serde_json::Value>) -> Option<Self> {
        fn get<T: serde::de::DeserializeOwned>(
            claims: &serde_json::Map<String, serde_json::Value>,
            key: &str,
        ) -> Option<T> {
            serde_json::from_value(claims.get(key)?.clone()).ok()
        }
        Some(Self {
            pwf: get(claims, Self::PWF)?,
            sat: get(claims, Self::SAT)?,
            fam: get(claims, Self::FAM)?,
        })
    }

    /// The one session check every bearer path runs: family unrevoked,
    /// under the cap, and `user`'s row still admits it (#2119, #2247).
    pub(crate) async fn admits(
        &self,
        check: &SessionCheck<'_>,
        user: &crate::tenancy::auth::User,
    ) -> bool {
        let now = chrono::Utc::now().timestamp();
        if check
            .cap_secs
            .is_some_and(|cap| now >= self.sat.saturating_add(cap))
        {
            return false;
        }
        if let Some(store) = check.families {
            if crate::tenancy::jwt_lifecycle::family_revoked_in(store, &self.fam).await {
                return false;
            }
        }
        crate::tenancy::session::session_survives(
            check.pwf_secret,
            &self.pwf,
            self.sat,
            &user.password_hash,
            user.password_changed_at,
            user.sessions_revoked_at,
        )
    }

    /// When the family stops mattering: the absolute cap.
    fn ends_at(&self, auth: &JwtAuth) -> i64 {
        self.sat.saturating_add(auth.0.session_cap_secs)
    }
}

impl JwtAuth {
    fn session_check(&self) -> SessionCheck<'_> {
        SessionCheck {
            pwf_secret: &self.0.pwf_secret,
            families: Some(self.0.jwt.jti_store()),
            cap_secs: Some(self.0.session_cap_secs),
        }
    }
}

/// The login pair for `user` on `slug`: hook claims, then the router's own.
fn issue_login_pair(
    auth: &JwtAuth,
    user: &crate::tenancy::auth::User,
    user_id: i64,
    slug: &str,
) -> Result<crate::tenancy::jwt_lifecycle::JwtTokenPair, crate::tenancy::jwt_lifecycle::JwtIssueError>
{
    let mut custom = login_claims(
        auth.0.extra_claims.as_ref(),
        &ClaimsContext {
            user_id,
            username: &user.username,
            is_superuser: user.is_superuser,
            tenant_slug: slug,
        },
    );
    // A `kind` claim marks a non-user token, which every bearer check refuses.
    let kind = crate::tenancy::jwt_lifecycle::CLAIM_KIND;
    if custom.contains_key(kind) {
        return Err(crate::tenancy::jwt_lifecycle::JwtIssueError::ReservedClaim(
            kind.to_owned(),
        ));
    }
    RefreshSession::start(auth, user).write(&mut custom);
    auth.lifecycle().issue_pair_with(user_id, custom)
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

fn login<DB: Database>(
    State(auth): State<JwtAuth>,
    t: Tenant<DB>,
    ip: crate::login_throttle::ClientIp,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginInput>,
) -> impl std::future::Future<Output = Result<Json<LoginOutput>, Response>> + Send {
    login_in(auth, t.into(), ip, extensions, headers, body)
}

async fn login_in(
    auth: JwtAuth,
    t: TenantScope,
    ip: crate::login_throttle::ClientIp,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    body: LoginInput,
) -> Result<Json<LoginOutput>, Response> {
    use crate::core::Column as _;
    use crate::login_throttle::LoginRefused;
    use crate::signals::auth::{
        meta_from_parts, send_user_logged_in, send_user_login_failed, AuthFailureReason,
        UserLoggedInContext, UserLoginFailedContext,
    };
    use crate::sql::FetcherPool as _;
    use crate::tenancy::auth::User;

    let meta = meta_from_parts(&extensions, &headers, Some("/auth/login"));
    let fire_failed = |reason: AuthFailureReason| -> UserLoginFailedContext {
        UserLoginFailedContext {
            source: "jwt",
            attempted_username: Some(body.username.clone()),
            reason,
            request: meta.clone(),
        }
    };

    // Rate limits and the account lock, before the lookup (#1609). Same
    // scope as the tenant admin login, which shares the user table.
    let mut attempt = crate::login_throttle::shared()
        .begin(
            &crate::login_throttle::LoginScope::Tenant(t.org.slug.clone()),
            &ip,
            &body.username,
        )
        .await
        .map_err(LoginRefused::into_response)?;
    let busy = |e: crate::tenancy::TenancyError| match e {
        crate::tenancy::TenancyError::Busy => LoginRefused::Busy.into_response(),
        e => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    let users = User::objects()
        .where_(User::username.eq(body.username.clone()))
        .fetch(t.pool())
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let Some(mut user) = users.into_iter().next() else {
        // H1: spend a verify's worth of work on the unknown-user path so
        // timing doesn't reveal whether the username exists.
        crate::tenancy::password::verify_dummy_async(&body.password)
            .await
            .map_err(busy)?;
        attempt.failed().await;
        send_user_login_failed(fire_failed(AuthFailureReason::InvalidCredentials)).await;
        return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
    };
    attempt
        .resolve(&user.username)
        .await
        .map_err(LoginRefused::into_response)?;
    let uid = user.id.get().copied().unwrap_or(0);

    // Verify before the active check so active vs inactive accounts take
    // the same time (audit H1).
    let ok = crate::tenancy::password::verify_async(&body.password, &user.password_hash)
        .await
        .map_err(busy)?;

    if !user.active {
        attempt.failed().await;
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
        attempt.failed().await;
        send_user_login_failed(fire_failed(AuthFailureReason::InvalidCredentials)).await;
        return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
    }

    attempt.succeeded().await;
    user.password_hash = crate::passwords::upgrade_stored_hash(
        t.pool(),
        <User as crate::core::Model>::SCHEMA,
        uid,
        &body.password,
        &user.password_hash,
    )
    .await;

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
    let pair = issue_login_pair(&auth, &user, user_id, &t.org.slug)
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

/// Rotate a refresh token. Each is single use; replaying a rotated one
/// revokes the whole chain. A password change or the absolute cap
/// (`Config::refresh_absolute_ttl_secs`) also ends it (#1854).
fn refresh<DB: Database>(
    State(auth): State<JwtAuth>,
    t: Tenant<DB>,
    Json(body): Json<RefreshInput>,
) -> impl std::future::Future<Output = Result<Json<RefreshOutput>, Response>> + Send {
    refresh_in(auth, t.into(), body)
}

async fn refresh_in(
    auth: JwtAuth,
    t: TenantScope,
    body: RefreshInput,
) -> Result<Json<RefreshOutput>, Response> {
    let refused = || err(StatusCode::UNAUTHORIZED, "invalid or expired refresh token");
    let jwt = auth.lifecycle();
    // Revocation is checked by the rotation below; decoding without it
    // lets a replayed token be told apart and its family revoked (#1854).
    let claims = jwt.decode_refresh(&body.refresh).ok_or_else(refused)?;
    // Audit N3 — bind refresh to the resolved tenant. Without this, a
    // refresh token minted on tenant A could be POSTed to tenant B's
    // /refresh (they share the session secret) and rotated — burning A's
    // refresh token (a cross-tenant DoS / rotation oracle). Verify the
    // token's `tenant` claim matches this subdomain BEFORE rotating.
    let tenant_ok =
        claims.custom_value("tenant").and_then(|v| v.as_str()) == Some(t.org.slug.as_str());
    if !tenant_ok {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "refresh token issued for a different tenant",
        ));
    }
    // Pre-#1854 tokens carry no session claims: fail closed.
    let session = RefreshSession::read(&claims.custom).ok_or_else(refused)?;
    // Audit P2 — re-check the account is still active (and exists) before
    // minting a fresh pair; #1854 — and that its session is still live.
    // Same uniform 401 as other refresh failures.
    {
        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        use crate::tenancy::auth::User;
        let users: Vec<User> = User::objects()
            .where_(User::id.eq(claims.sub))
            .fetch(t.pool())
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let Some(user) = users.into_iter().next().filter(|u| u.active) else {
            return Err(refused());
        };
        if !session.admits(&auth.session_check(), &user).await {
            return Err(refused());
        }
    }
    let grace = auth.0.reuse_grace_secs;
    jwt.note_refresh_attempt(&claims.jti, grace).await;
    let Some(pair) = jwt.refresh(&body.refresh).await else {
        // Already redeemed. A retry just after rotation only gets the 401;
        // a later reuse is a stolen token, so end the chain.
        if !jwt.recently_attempted(&claims.jti, grace).await {
            jwt.revoke_family(&session.fam, session.ends_at(&auth))
                .await;
        }
        return Err(refused());
    };
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

fn logout<DB: Database>(
    State(auth): State<JwtAuth>,
    t: Tenant<DB>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    bearer: Bearer,
    body: Option<Json<LogoutInput>>,
) -> impl std::future::Future<Output = Result<StatusCode, Response>> + Send {
    logout_in(auth, t.into(), extensions, headers, bearer, body)
}

async fn logout_in(
    auth: JwtAuth,
    t: TenantScope,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    bearer: Bearer,
    body: Option<Json<LogoutInput>>,
) -> Result<StatusCode, Response> {
    let jwt = auth.lifecycle();
    use crate::signals::auth::{meta_from_parts, send_user_logged_out, UserLoggedOutContext};
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
    let meta = meta_from_parts(&extensions, &headers, Some("/auth/logout"));
    let refresh = body.and_then(|Json(input)| input.refresh);

    // The family too, so every token of this login ends (#2119). Read from
    // any token signed for this tenant, expired or already rotated, so a
    // thief's rotated refresh dies with it (#2419).
    for token in std::iter::once(bearer.0.as_str()).chain(refresh.as_deref()) {
        let Some(c) = jwt.decode_signed(token) else {
            continue;
        };
        if c.custom_value("tenant").and_then(|v| v.as_str()) != Some(t.org.slug.as_str()) {
            continue;
        }
        if let Some(session) = RefreshSession::read(&c.custom) {
            jwt.revoke_family(&session.fam, session.ends_at(&auth))
                .await;
        }
    }

    jwt.revoke(&bearer.0).await;

    // The refresh token too, when we were given one. Revoking it is what
    // actually ends the session: the access token expires on its own in
    // minutes, the refresh token would have outlived the logout by days
    // and could mint replacements the whole time.
    //
    // Tenant-pinned the same way the bearer is, so one subdomain cannot
    // revoke another tenant's token by posting it here.
    if let Some(refresh) = refresh.as_deref() {
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
/// request tenant so a JWT minted on one subdomain can't be replayed
/// against another, and refuses a token whose session has ended (#2086).
fn me<DB: Database>(
    State(auth): State<JwtAuth>,
    t: Tenant<DB>,
    bearer: Bearer,
) -> impl std::future::Future<Output = Result<Json<UserBrief>, Response>> + Send {
    me_in(auth, t.into(), bearer)
}

async fn me_in(auth: JwtAuth, t: TenantScope, bearer: Bearer) -> Result<Json<UserBrief>, Response> {
    let user = session_user(&auth, &bearer.0, &t)
        .await?
        .ok_or_else(|| unauthorized("invalid or expired token"))?;

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
/// 1. signature, expiry, `typ = access`, and the JTI revocation list;
/// 2. the token's `tenant` claim against the resolved tenant — all tenants
///    share one signing key, so this is the only thing standing between a
///    token minted on `acme.` and a request to `globex.`;
/// 3. the user row, **read per request**, so deactivating an account, a
///    password change or a logout takes effect immediately rather than
///    whenever the access token happens to expire. Only tokens minted by
///    `/login` or `/refresh` pass: they carry the session it checks (#2086).
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
pub fn require_bearer(
    auth: State<JwtAuth>,
    t: Tenant,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl std::future::Future<Output = Response> + Send {
    require_bearer_for::<DefaultTenantDb>(auth, t, req, next)
}

/// [`require_bearer`] for a `Tenant<DB>` other than the default (#1778).
pub fn require_bearer_for<DB: Database>(
    State(auth): State<JwtAuth>,
    t: Tenant<DB>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl std::future::Future<Output = Response> + Send {
    bearer_in(auth, t.into(), req, next)
}

async fn bearer_in(
    auth: JwtAuth,
    t: TenantScope,
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

    let user = match session_user(&auth, token, &t).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Some(user) = user.filter(|u| u.active) else {
        // Deleted or deactivated between mint and use. Same body as a bad
        // token: whether an account exists is not a fact this endpoint owes.
        return unauthorized("invalid or expired token");
    };

    let Some(id) = user.id.get().copied() else {
        return unauthorized("invalid or expired token");
    };
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

/// The user a tenant access token acts as. `Ok(None)` when the row is gone
/// or the token's session has ended: a logout or password change (#2086).
async fn session_user(
    auth: &JwtAuth,
    token: &str,
    t: &TenantScope,
) -> Result<Option<crate::tenancy::auth::User>, Response> {
    // The reason is deliberately not echoed: "expired" vs "wrong tenant"
    // vs "revoked" tells a prober which of those they achieved.
    session_user_in(auth, token, &t.org.slug, t.pool())
        .await
        .map_err(|e| match e {
            SessionLookup::Refused => unauthorized("invalid or expired token"),
            SessionLookup::Db(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        })
}

/// Why [`session_user_in`] found no user to act as.
enum SessionLookup {
    Refused,
    Db(crate::sql::ExecError),
}

async fn session_user_in(
    auth: &JwtAuth,
    token: &str,
    slug: &str,
    pool: &crate::sql::Pool,
) -> Result<Option<crate::tenancy::auth::User>, SessionLookup> {
    let claims = auth
        .0
        .jwt
        .verify_access(token)
        .await
        .ok_or(SessionLookup::Refused)?;
    let user_id = claims
        .user_id_in(UserTokenScope::Tenant(slug))
        .map_err(|_| SessionLookup::Refused)?;
    // Tokens the login route did not mint carry no session: fail closed.
    let Some(session) = RefreshSession::read(&claims.custom) else {
        return Ok(None);
    };
    let users: Vec<crate::tenancy::auth::User> = crate::tenancy::auth::User::objects()
        .filter("id", user_id)
        .fetch(pool)
        .await
        .map_err(SessionLookup::Db)?;
    let Some(user) = users.into_iter().next() else {
        return Ok(None);
    };
    Ok(session
        .admits(&auth.session_check(), &user)
        .await
        .then_some(user))
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

    /// A `Tenant` for `testkit::org()` over `pool`, for `verify_for_tenant`.
    #[cfg(feature = "sqlite")]
    async fn sqlite_tenant(pool: &crate::sql::Pool) -> Tenant<sqlx::Sqlite> {
        let conn = pool.as_sqlite().unwrap().acquire().await.unwrap();
        let conn = crate::tenancy::TenantConn::database(conn);
        Tenant::for_test(crate::testkit::org(), conn, pool.clone())
    }

    /// A shared in-memory SQLite, so the held `Tenant` conn and the pool agree.
    #[cfg(feature = "sqlite")]
    async fn verify_env(name: &str) -> (JwtAuth, crate::sql::Pool, crate::tenancy::auth::User) {
        let url = format!("sqlite:file:{name}?mode=memory&cache=shared");
        let pool = crate::sql::Pool::connect(&url).await.unwrap();
        crate::testkit::create_tables_for::<crate::tenancy::auth::User>(&pool)
            .await
            .unwrap();
        let mut user = crate::testkit::user();
        user.insert_pool(&pool).await.unwrap();
        let auth = JwtAuth::new(Config {
            session_secret: Some(vec![7; 32]),
            ..Config::default()
        });
        (auth, pool, user)
    }

    #[cfg(feature = "sqlite")]
    fn login_access(auth: &JwtAuth, user: &crate::tenancy::auth::User) -> String {
        let id = user.id.get().copied().unwrap();
        issue_login_pair(auth, user, id, &crate::testkit::org().slug)
            .unwrap()
            .access
    }

    /// Two configs stay two. Under the old `OnceLock` the first one's key
    /// signed for both, so B accepted A's token (#1190).
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn each_jwt_auth_keeps_its_own_key() {
        let (a, pool, user) = verify_env("jwt_own_key_1190").await;
        let b = JwtAuth::new(Config {
            session_secret: Some(b"key-b-key-b-key-b-key-b-key-b-32".to_vec()),
            ..Config::default()
        });
        let t = sqlite_tenant(&pool).await;
        let token = login_access(&a, &user);

        assert_eq!(
            a.verify_for_tenant(&token, &t).await,
            Ok(*user.id.get().unwrap())
        );
        assert!(b.verify_for_tenant(&token, &t).await.is_err());
    }

    /// #1848 — an MCP agent token from the same lifecycle is not a user bearer.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn an_agent_token_is_not_a_user_bearer() {
        let (auth, pool, user) = verify_env("jwt_agent_1848").await;
        let t = sqlite_tenant(&pool).await;
        let mut custom = serde_json::Map::new();
        custom.insert("tenant".into(), crate::testkit::org().slug.into());
        custom.insert("kind".into(), "agent".into());
        let id = *user.id.get().unwrap();
        let token = auth.lifecycle().issue_access_with(id, custom).unwrap();

        assert!(auth.verify_for_tenant(&token, &t).await.is_err());
    }

    /// #2118 — a logout or a password change ends a token for `verify_for_tenant` too.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn verify_for_tenant_refuses_an_ended_session() {
        let (auth, pool, mut user) = verify_env("jwt_verify_2118").await;
        let t = sqlite_tenant(&pool).await;
        let id = *user.id.get().unwrap();

        let token = login_access(&auth, &user);
        assert_eq!(auth.verify_for_tenant(&token, &t).await, Ok(id));
        user.password_hash = "$argon2id$changed".into();
        user.save_pool(&pool).await.unwrap();
        assert!(
            auth.verify_for_tenant(&token, &t).await.is_err(),
            "password change"
        );

        let token = login_access(&auth, &user);
        assert_eq!(auth.verify_for_tenant(&token, &t).await, Ok(id));
        crate::session::revoke_sessions::<crate::tenancy::auth::User>(&pool, id, None, 0)
            .await
            .unwrap();
        assert!(auth.verify_for_tenant(&token, &t).await.is_err(), "logout");
    }

    /// #2118 review — a deactivated user's token is refused.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn verify_for_tenant_refuses_an_inactive_user() {
        let (auth, pool, mut user) = verify_env("jwt_verify_inactive_2118").await;
        let t = sqlite_tenant(&pool).await;
        let token = login_access(&auth, &user);
        assert!(auth.verify_for_tenant(&token, &t).await.is_ok());
        user.active = false;
        user.save_pool(&pool).await.unwrap();
        assert_eq!(
            auth.verify_for_tenant(&token, &t).await,
            Err("invalid or expired token")
        );
    }

    /// A tenant pool with one user, and a `JwtAuth` capped at `cap` seconds.
    #[cfg(feature = "sqlite")]
    async fn refresh_env(cap: i64) -> (JwtAuth, crate::sql::Pool, crate::tenancy::auth::User) {
        refresh_env_with(cap, 0).await
    }

    #[cfg(feature = "sqlite")]
    async fn refresh_env_with(
        cap: i64,
        grace: i64,
    ) -> (JwtAuth, crate::sql::Pool, crate::tenancy::auth::User) {
        let pool = crate::sql::Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::create_tables_for::<crate::tenancy::auth::User>(&pool)
            .await
            .unwrap();
        let mut user = crate::tenancy::auth::User {
            password_hash: "$argon2id$old".into(),
            ..crate::testkit::user()
        };
        user.insert_pool(&pool).await.unwrap();
        let auth = JwtAuth::new(Config {
            session_secret: Some(vec![7; 32]),
            refresh_absolute_ttl_secs: cap,
            refresh_reuse_grace_secs: grace,
            ..Config::default()
        });
        (auth, pool, user)
    }

    #[cfg(feature = "sqlite")]
    async fn rotate(auth: &JwtAuth, pool: &crate::sql::Pool, refresh: &str) -> Option<String> {
        let scope = TenantScope::for_test(crate::testkit::org(), pool.clone());
        let body = RefreshInput {
            refresh: refresh.to_owned(),
        };
        refresh_in(auth.clone(), scope, body)
            .await
            .ok()
            .map(|Json(p)| p.refresh)
    }

    #[cfg(feature = "sqlite")]
    fn login(auth: &JwtAuth, user: &crate::tenancy::auth::User) -> String {
        let id = user.id.get().copied().unwrap();
        issue_login_pair(auth, user, id, &crate::testkit::org().slug)
            .unwrap()
            .refresh
    }

    /// #1854 — a password change ends the refresh chain.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_password_change_ends_the_refresh_chain() {
        let (auth, pool, mut user) = refresh_env(3600).await;
        let first = login(&auth, &user);
        let second = rotate(&auth, &pool, &first).await.expect("rotates before");

        user.password_hash = "$argon2id$new".into();
        user.save_pool(&pool).await.unwrap();

        assert_eq!(rotate(&auth, &pool, &second).await, None);
    }

    /// #2036 — a logout's cut-off ends refresh chains started before it.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_logout_ends_the_refresh_chain() {
        let (auth, pool, mut user) = refresh_env(3600).await;
        let first = login(&auth, &user);
        let second = rotate(&auth, &pool, &first).await.expect("rotates before");

        let id = user.id.get().copied().unwrap();
        crate::session::revoke_sessions::<crate::tenancy::auth::User>(&pool, id, None, 0)
            .await
            .unwrap();
        assert_eq!(rotate(&auth, &pool, &second).await, None, "chain ended");

        // A login right after the logout, even in its second, refreshes.
        user = crate::tenancy::auth::User::objects()
            .filter("id", id)
            .fetch(&pool)
            .await
            .unwrap()
            .remove(0);
        let fresh = login(&auth, &user);
        assert!(rotate(&auth, &pool, &fresh).await.is_some());
    }

    /// #1854 — the cap counts from login, not from the last rotation.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn the_absolute_cap_survives_rotation() {
        let (auth, pool, user) = refresh_env(2).await;
        let first = login(&auth, &user);
        let second = rotate(&auth, &pool, &first)
            .await
            .expect("rotates inside the cap");

        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        assert_eq!(rotate(&auth, &pool, &second).await, None);
    }

    /// #1854 — replaying a rotated token revokes the chain it belongs to.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn replaying_a_rotated_token_revokes_the_family() {
        let (auth, pool, user) = refresh_env(3600).await;
        let first = login(&auth, &user);
        let second = rotate(&auth, &pool, &first).await.expect("rotates");

        assert_eq!(rotate(&auth, &pool, &first).await, None, "replay refused");
        assert_eq!(rotate(&auth, &pool, &second).await, None, "chain revoked");

        let other = login(&auth, &user);
        assert!(
            rotate(&auth, &pool, &other).await.is_some(),
            "other logins live"
        );
    }

    /// Review of #1854 — a retry of a just-rotated token keeps the chain alive.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_retry_inside_the_grace_window_keeps_the_family() {
        let (auth, pool, user) = refresh_env_with(3600, 10).await;
        let first = login(&auth, &user);
        let second = rotate(&auth, &pool, &first).await.expect("rotates");

        assert_eq!(rotate(&auth, &pool, &first).await, None, "retry still 401s");
        assert!(rotate(&auth, &pool, &second).await.is_some(), "chain alive");
    }

    /// Past the window a reuse is theft again.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_replay_after_the_grace_window_revokes_the_family() {
        let (auth, pool, user) = refresh_env_with(3600, 1).await;
        let first = login(&auth, &user);
        let second = rotate(&auth, &pool, &first).await.expect("rotates");

        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        assert_eq!(rotate(&auth, &pool, &first).await, None);
        assert_eq!(rotate(&auth, &pool, &second).await, None, "chain revoked");
    }

    /// Two concurrent refreshes of one token: one wins, the chain survives.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn concurrent_refreshes_keep_the_family() {
        let (auth, pool, user) = refresh_env_with(3600, 10).await;
        let first = login(&auth, &user);
        let (a, b) = tokio::join!(rotate(&auth, &pool, &first), rotate(&auth, &pool, &first));
        let winner = a.or(b).expect("one wins");
        assert!(rotate(&auth, &pool, &winner).await.is_some(), "chain alive");
    }

    /// #2247 — `JwtBackend` ends a login token with its session, like `require_bearer`.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn jwt_backend_refuses_an_ended_login_session() {
        use crate::tenancy::auth_backends::{AuthBackend as _, AuthError, JwtBackend};
        let store: Arc<dyn crate::jti_store::JtiStore> =
            Arc::new(crate::jti_store::InMemoryJtiStore::new());
        let pool = crate::sql::Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::create_tables_for::<crate::tenancy::auth::User>(&pool)
            .await
            .unwrap();
        let mut user = crate::testkit::user();
        user.insert_pool(&pool).await.unwrap();
        let id = *user.id.get().unwrap();
        let auth = JwtAuth::new(Config {
            session_secret: Some(vec![7; 32]),
            jti_store: Some(store.clone()),
            refresh_reuse_grace_secs: 0,
            ..Config::default()
        });
        let backend = JwtBackend::new(vec![7; 32]).with_jti_store(store);
        let slug = crate::testkit::org().slug;
        let check = |token: String| {
            let (backend, pool, slug) = (&backend, &pool, slug.clone());
            async move {
                let (mut parts, ()) = axum::http::Request::builder()
                    .header("authorization", format!("Bearer {token}"))
                    .body(())
                    .unwrap()
                    .into_parts();
                parts.extensions.insert(crate::tenancy::TenantSlug(slug));
                backend
                    .authenticate(&parts, pool)
                    .await
                    .map(|u| u.map(|u| u.id))
            }
        };
        let pair = |user: &crate::tenancy::auth::User| {
            issue_login_pair(&auth, user, id, &crate::testkit::org().slug).unwrap()
        };

        let p = pair(&user);
        assert_eq!(check(p.access.clone()).await.ok(), Some(Some(id)));
        rotate(&auth, &pool, &p.refresh).await.expect("rotates");
        assert_eq!(rotate(&auth, &pool, &p.refresh).await, None, "replay");
        assert!(
            matches!(check(p.access).await, Err(AuthError::InvalidToken)),
            "family"
        );

        let access = pair(&user).access;
        user.password_hash = "$argon2id$changed".into();
        user.save_pool(&pool).await.unwrap();
        assert!(
            matches!(check(access).await, Err(AuthError::InvalidToken)),
            "password"
        );

        let access = pair(&user).access;
        assert_eq!(check(access.clone()).await.ok(), Some(Some(id)));
        crate::session::revoke_sessions::<crate::tenancy::auth::User>(&pool, id, None, 0)
            .await
            .unwrap();
        assert!(
            matches!(check(access).await, Err(AuthError::InvalidToken)),
            "logout-all"
        );
    }

    /// A hook's `kind` claim would make every token refused, so login fails.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_kind_claim_from_the_hook_fails_the_login() {
        let hook: ClaimsHook = Arc::new(|_: &ClaimsContext<'_>| {
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), "agent".into());
            m
        });
        let auth = JwtAuth::new(Config {
            session_secret: Some(vec![7; 32]),
            extra_claims: Some(hook),
            ..Config::default()
        });
        let user = crate::tenancy::auth::User {
            id: crate::sql::Auto::Set(1),
            ..crate::testkit::user()
        };
        assert!(issue_login_pair(&auth, &user, 1, "acme").is_err());
    }

    #[test]
    #[should_panic(expected = "refresh_absolute_ttl_secs must be > 0")]
    fn a_zero_session_cap_is_refused() {
        let _ = JwtAuth::new(Config {
            session_secret: Some(vec![7; 32]),
            refresh_absolute_ttl_secs: 0,
            ..Config::default()
        });
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
