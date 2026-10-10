//! Pluggable authentication backends.
//!
//! Register one or more backends with the server builder or tenant admin;
//! requests are authenticated by trying each backend in registration order.
//! First `Ok(Some(user))` wins; all `Ok(None)` → anonymous.
//!
//! # Built-in backends
//!
//! | Backend | How it reads the identity |
//! |---|---|
//! | [`ModelBackend`] | `Authorization: Basic <b64(user:pass)>` — or form fields `username` / `password` |
//! | [`ApiKeyBackend`] | `Authorization: Bearer <key>` against `rustango_api_keys` |
//! | [`JwtBackend`] | `Authorization: Bearer <jwt>` — HMAC-SHA256 signed, user id in `sub` |
//!
//! # Custom backend
//!
//! ```ignore
//! use rustango::tenancy::auth_backends::{AuthBackend, AuthUser, AuthError};
//! use async_trait::async_trait;
//!
//! pub struct MyBackend;
//!
//! #[async_trait]
//! impl AuthBackend for MyBackend {
//!     async fn authenticate(
//!         &self,
//!         parts: &axum::http::request::Parts,
//!         pool: &rustango::sql::sqlx::PgPool,
//!     ) -> Result<Option<AuthUser>, AuthError> {
//!         // read a custom header, verify, return Some(AuthUser { .. })
//!         Ok(None)
//!     }
//! }
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::request::Parts;

use crate::sql::sqlx;
use crate::sql::Pool;

use super::auth::parse_basic_auth;
use super::password;
use super::password::HashLane;
use crate::login_throttle::{ClientIp, LoginAttempt, LoginScope};

// ------------------------------------------------------------------ AuthUser

/// Authenticated identity returned by a successful backend.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// `rustango_users.id`
    pub id: i64,
    /// Login handle.
    pub username: String,
    /// Org-admin within this tenant.
    pub is_superuser: bool,
}

// ------------------------------------------------------------------ AuthError

/// Error returned by a backend that hard-fails (as opposed to `Ok(None)`
/// which means "I can't handle this request, try the next backend").
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database error: {0}")]
    Exec(#[from] crate::sql::ExecError),
    #[error("token is malformed or expired")]
    InvalidToken,
    #[error("account is inactive")]
    Inactive,
    /// Locked out or password hashing busy; answer with its response.
    #[error("login refused: {0:?}")]
    Refused(crate::login_throttle::LoginRefused),
}

impl AuthError {
    /// A hashing error: busy is [`LoginRefused::Busy`], anything else an
    /// invalid credential.
    ///
    /// [`LoginRefused::Busy`]: crate::login_throttle::LoginRefused::Busy
    fn from_hash(e: super::TenancyError) -> Self {
        match e {
            super::TenancyError::Busy => Self::Refused(crate::login_throttle::LoginRefused::Busy),
            _ => Self::InvalidToken,
        }
    }
}

/// Admit one per-request credential check for this request's tenant and
/// client. Refuses without a tenant, so tenants never share limits.
async fn begin(
    parts: &Parts,
    scope: fn(String) -> LoginScope,
    username: &str,
) -> Result<LoginAttempt, AuthError> {
    let Some(slug) = parts.extensions.get::<super::TenantSlug>() else {
        tracing::error!("auth backend ran without a TenantSlug; use require_auth / optional_auth");
        return Err(AuthError::InvalidToken);
    };
    let ip = parts
        .extensions
        .get::<ClientIp>()
        .cloned()
        .unwrap_or_else(|| ClientIp::from_parts(&parts.extensions, &parts.headers));
    crate::login_throttle::shared()
        .begin(&scope(slug.0.clone()), &ip, username)
        .await
        .map_err(AuthError::Refused)
}

// ------------------------------------------------------------------ Trait

/// An authentication backend. Implement this to add custom auth strategies.
///
/// Each backend is called in registration order. Return:
/// - `Ok(Some(user))` — authentication succeeded.
/// - `Ok(None)` — this backend doesn't handle the request; try the next one.
/// - `Err(_)` — hard failure (wrong password, expired token, DB error).
#[async_trait]
pub trait AuthBackend: Send + Sync {
    async fn authenticate(&self, parts: &Parts, pool: &Pool)
        -> Result<Option<AuthUser>, AuthError>;
}

/// Heap-allocated dyn backend.
pub type BoxedBackend = Arc<dyn AuthBackend>;

// ------------------------------------------------------------------ ModelBackend

/// Username + password backend. Reads `Authorization: Basic <b64>` and
/// verifies against `rustango_users` with argon2id.
///
/// This is the default backend.
pub struct ModelBackend;

#[async_trait]
impl AuthBackend for ModelBackend {
    async fn authenticate(
        &self,
        parts: &Parts,
        pool: &Pool,
    ) -> Result<Option<AuthUser>, AuthError> {
        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;

        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());

        let (username, password) = match parse_basic_auth(auth_header) {
            Some(pair) => pair,
            None => return Ok(None),
        };

        // Own lock scope, apart from the login forms; only failures count.
        let mut attempt = begin(parts, LoginScope::TenantBasic, &username).await?;

        let users = super::auth::User::objects()
            .where_(super::auth::User::username.eq(username.clone()))
            .fetch(pool)
            .await?;

        let Some(user) = users.into_iter().next() else {
            // Audit H1/N4 — spend a verify's worth of work on the
            // unknown-user path so timing doesn't reveal whether the
            // username exists.
            password::verify_dummy_async_in(HashLane::Credential, &password)
                .await
                .map_err(AuthError::from_hash)?;
            attempt.failed().await;
            return Ok(None);
        };
        attempt
            .resolve(&user.username)
            .await
            .map_err(AuthError::Refused)?;

        // Verify before the active check so active vs inactive accounts
        // take the same time (audit H1/N4).
        let ok = password::verify_async_in(HashLane::Credential, &password, &user.password_hash)
            .await
            .map_err(AuthError::from_hash)?;
        if !user.active || !ok {
            attempt.failed().await;
            // Audit N4 — an inactive account must look identical to a
            // wrong password at this (username-keyed, pre-credential)
            // boundary: same `Ok(None)`, not a distinguishable
            // `Err(Inactive)`, so the backend can't be used to enumerate
            // accounts. (Inactive is still enforced after a *valid* API
            // key / JWT below, where the caller already proved ownership.)
            return Ok(None);
        }

        attempt.succeeded().await;
        Ok(Some(AuthUser {
            id: user.id.get().copied().unwrap_or(0),
            username: user.username,
            is_superuser: user.is_superuser,
        }))
    }
}

// ------------------------------------------------------------------ ApiKey model + backend

/// An API key for a tenant user. Keys are stored hashed; the full
/// token is only returned at creation time.
///
/// Query with the ORM:
/// ```ignore
/// let keys = ApiKey::objects()
///     .where_(ApiKey::user_id.eq(alice_id))
///     .fetch(&pool)
///     .await?;
/// ```
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_api_keys",
    admin(
        list_display = "user_id, key_prefix, label, expires_at, created_at",
        ordering = "-created_at",
        readonly_fields = "key_prefix, key_hash, created_at",
    )
)]
pub struct ApiKey {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,
    /// `rustango_users.id`
    #[rustango(fk = "rustango_users", on = "id", on_delete = "cascade")]
    pub user_id: i64,
    /// 8-char hex prefix — public, used to look up the row. Stored in
    /// plaintext by design (it's the lookup index). Audit L4: this is an
    /// accepted, minor exposure — a DB leak reveals which prefixes exist
    /// but NOT the secret (the secret half is argon2id-hashed in
    /// `key_hash`), so a stolen prefix can't authenticate on its own.
    ///
    /// Indexed: every Bearer request looks the row up by this column,
    /// so without one it is a sequential scan on the hit path — 5.9 ms
    /// at 100k keys against 0.025 ms indexed, growing with every key
    /// ever issued (#1647).
    #[rustango(max_length = 8, index)]
    pub key_prefix: String,
    /// argon2id hash of the 32-char secret. Never returned to callers.
    #[rustango(max_length = 255)]
    pub key_hash: String,
    /// Human-readable label (e.g. "CI pipeline key").
    #[rustango(max_length = 100)]
    pub label: String,
    /// Optional expiry. `None` = never expires.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set on INSERT via `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    pub created_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
}

// A key is minted by `create-api-key`, which shows the secret once; an admin
// form could only store a row nobody can use (#1763).
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_api_keys", "add", |_, _| false);
// The owner decides who the credential signs in as (#2520).
#[cfg(feature = "admin")]
crate::register_admin_superuser_fields!(ApiKey, Change, [user_id]);

/// Create the `rustango_api_keys` table if it doesn't exist.
///
/// PG-typed back-compat for legacy callers; new code should use
/// [`ensure_api_keys_table_pool`] which picks the right per-dialect
/// DDL constant via `pool.dialect().name()`.
///
/// #562 — delegates to [`ensure_api_keys_table_pool`] so the
/// statement-splitting loop lives in one place.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "postgres")]
pub async fn ensure_api_keys_table(pool: &crate::sql::sqlx::PgPool) -> Result<(), sqlx::Error> {
    ensure_api_keys_table_pool(&crate::sql::Pool::from(pool.clone())).await
}

/// v0.38 — tri-dialect counterpart of [`ensure_api_keys_table`].
/// Picks the per-dialect DDL constant based on `pool.dialect().name()`
/// and runs each statement separately (sqlx's simple-prepare rejects
/// multi-statement strings).
///
/// # Errors
/// Driver / SQL failures from `CREATE TABLE IF NOT EXISTS`.
pub async fn ensure_api_keys_table_pool(pool: &Pool) -> Result<(), sqlx::Error> {
    // Drift-free (v0.47): emit `rustango_api_keys` from `ApiKey::SCHEMA`
    // via the migration render path instead of hand-written per-dialect
    // DDL. Idempotent (swallows "already exists").
    use crate::core::Model as _;
    let snapshot = crate::migrate::SchemaSnapshot::from_models(&[ApiKey::SCHEMA]);
    crate::migrate::apply_idempotent(pool, &snapshot).await?;
    Ok(())
}

/// `Authorization: Bearer <key>` backend. Keys are stored hashed;
/// the bearer token format is `<8-char prefix>.<secret>`.
///
/// Generate a key with `cargo run -- create-api-key <username>`.
pub struct ApiKeyBackend;

#[async_trait]
impl AuthBackend for ApiKeyBackend {
    async fn authenticate(
        &self,
        parts: &Parts,
        pool: &Pool,
    ) -> Result<Option<AuthUser>, AuthError> {
        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;

        let bearer = extract_bearer(parts)?;
        let Some(token) = bearer else {
            return Ok(None);
        };

        // Format: "<prefix>.<secret>"
        let (prefix, secret) = match token.split_once('.') {
            Some(p) => p,
            None => return Ok(None), // Not an API key format
        };

        if prefix.len() != 8 {
            return Ok(None);
        }

        // Two indexed ORM round-trips: the key by prefix, then its user.
        // Failures per IP and per tenant; no lock, the prefix is no account.
        let attempt = begin(parts, LoginScope::TenantApiKey, "").await?;
        let keys = ApiKey::objects()
            .where_(ApiKey::key_prefix.eq(prefix.to_owned()))
            .fetch(pool)
            .await?;
        // The prefix is random, not unique: try every row (#2250). An unknown
        // prefix still spends a verify (audit N4).
        let key = password::first_verified(keys, secret, |k| &k.key_hash)
            .await
            .map_err(AuthError::from_hash)?;
        let Some(key) = key else {
            attempt.failed().await;
            return Ok(None);
        };

        // Verify before judging expiry, so an expired prefix costs the
        // same as an unknown one (#1729).
        if key.expires_at.is_some_and(|exp| chrono::Utc::now() > exp) {
            return Err(AuthError::InvalidToken);
        }
        attempt.succeeded().await;

        let users = super::auth::User::objects()
            .where_(super::auth::User::id.eq(key.user_id))
            .fetch(pool)
            .await?;
        let Some(user) = users.into_iter().next() else {
            return Ok(None);
        };
        if !user.active {
            return Err(AuthError::Inactive);
        }

        Ok(Some(AuthUser {
            id: user.id.get().copied().unwrap_or(0),
            username: user.username,
            is_superuser: user.is_superuser,
        }))
    }
}

/// Create an API key for `user_id`. Returns the full token (`prefix.secret`)
/// — this is the only time the plaintext is available.
///
/// # Errors
/// Driver errors.
pub async fn create_api_key(
    user_id: i64,
    label: &str,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pool: &Pool,
) -> Result<String, crate::tenancy::error::TenancyError> {
    use crate::sql::Auto;
    use rand::rngs::OsRng;
    use rand::RngCore;

    // v0.42 — API key prefix + secret source from the OS CSPRNG. The
    // 16-byte secret is the actual bearer credential; a predictable
    // RNG here would compromise every key issued by the affected
    // process.
    let mut prefix_bytes = [0u8; 4];
    OsRng.fill_bytes(&mut prefix_bytes);
    let prefix = to_hex(&prefix_bytes);
    let mut secret_bytes = [0u8; 16];
    OsRng.fill_bytes(&mut secret_bytes);
    let secret = to_hex(&secret_bytes);

    let hash = password::hash_async(&secret)
        .await
        .map_err(|e| crate::tenancy::error::TenancyError::Validation(e.to_string()))?;

    let mut key = ApiKey {
        id: Auto::default(),
        user_id,
        key_prefix: prefix.clone(),
        key_hash: hash,
        label: label.to_owned(),
        expires_at,
        created_at: Auto::default(),
    };
    key.save_pool(pool).await?;

    Ok(format!("{prefix}.{secret}"))
}

// ------------------------------------------------------------------ JwtBackend

/// HMAC-SHA256 signed JWT backend. The JWT payload is
/// `{"sub": <user_id>, "exp": <unix_seconds>}`. Uses the same
/// [`SessionSecret`] as the admin session cookie.
///
/// [`SessionSecret`]: super::operator_console::SessionSecret
///
/// v0.38 — `from_session_secret` + the verify/issue impls depend on
/// `operator_console::session::sign` which is PG-gated until slice 5.
/// The backend struct + `new()` stay unconditional so projects can
/// still register it (sqlite/mysql apps would build the secret bytes
/// themselves until then).
pub struct JwtBackend {
    secret: Vec<u8>,
    /// Same key, for the login-session `pwf` claim (#2247).
    pwf_secret: crate::session::SessionSecret,
    /// Token lifetime in seconds for tokens issued via [`JwtBackend::issue`].
    pub ttl_secs: i64,
    /// Revocation list consulted on every authentication (#1402).
    ///
    /// `None` means revocation is not enforced on this backend, which is
    /// the pre-0.57.3 behaviour and stays the default only because
    /// turning it on silently would change what an existing deployment's
    /// tokens do. Set it with [`JwtBackend::with_jti_store`], passing the
    /// **same** store the issuing [`JwtLifecycle`] holds — two stores
    /// mean logout writes to one and verification reads the other.
    jti_store: Option<std::sync::Arc<dyn crate::jti_store::JtiStore>>,
    /// Set once this backend has warned that revocation is not checked.
    warned_unchecked: std::sync::atomic::AtomicBool,
}

impl JwtBackend {
    /// Build a backend from raw key bytes.
    ///
    /// # Panics
    /// If `secret` is shorter than 32 bytes (audit N5). HMAC accepts any
    /// key length, but a short/empty key is guessable and would let
    /// anyone forge tokens — fail closed rather than sign/verify with a
    /// weak key. Matches the 32-byte floor on `auth_routes::build_jwt`.
    #[must_use]
    pub fn new(secret: Vec<u8>) -> Self {
        // No `secret.len()` in the message — same reason as
        // `JwtLifecycle::new`: a panic message reaches logs and crash
        // reports, and the length of a key is information about it.
        assert!(
            secret.len() >= 32,
            "JwtBackend signing key is too short; need >= 32 bytes (a shorter key is forgeable)",
        );
        Self {
            pwf_secret: crate::session::SessionSecret::from_bytes(secret.clone()),
            secret,
            ttl_secs: 3600,
            jti_store: None,
            warned_unchecked: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Enforce revocation on this backend (#1402).
    ///
    /// Pass the **same** store the issuing
    /// [`JwtLifecycle`](crate::tenancy::jwt_lifecycle::JwtLifecycle)
    /// holds. Without
    /// this, `revoke()` and `/api/auth/logout` write to a blacklist that
    /// nothing on the authentication path reads — a revoked token keeps
    /// authenticating until it expires, and a deployment can watch a
    /// shared Redis store fill with JTIs that all still work.
    ///
    /// Without it, a single-login logout and a refresh-replay revoke go
    /// unseen; a password change and logout-all still end the token (#2247).
    ///
    /// Off by default because turning it on changes what an existing
    /// deployment's live tokens do, which is not a thing to do silently
    /// in a patch release.
    #[must_use]
    pub fn with_jti_store(mut self, store: std::sync::Arc<dyn crate::jti_store::JtiStore>) -> Self {
        self.jti_store = Some(store);
        self
    }

    /// A revocable token met a backend with no store: say so, once (#1809).
    fn warn_unchecked_revocation(&self) {
        use std::sync::atomic::Ordering;
        if !self.warned_unchecked.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "rustango::tenancy",
                "JwtBackend has no JTI store, so tokens are not checked for revocation: a \
                 logged-out token works until it expires; call `with_jti_store`"
            );
        }
    }

    /// Build from the operator-console session secret (convenient for
    /// projects that don't want a separate signing key).
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn from_session_secret(s: &super::operator_console::SessionSecret) -> Self {
        Self::new(s.key().to_vec())
    }

    /// Issue a signed JWT for `user_id` valid for `self.ttl_secs`, with no
    /// tenant binding: it authenticates only where no tenant is resolved.
    #[must_use]
    pub fn issue(&self, user_id: i64) -> String {
        self.sign_payload(&serde_json::json!({"sub": user_id, "exp": self.exp()}))
    }

    /// [`Self::issue`] bound to tenant `slug`, for `require_auth` routes (#1848).
    #[must_use]
    pub fn issue_for_tenant(&self, user_id: i64, slug: &str) -> String {
        let mut payload = serde_json::json!({"sub": user_id, "exp": self.exp()});
        payload[super::jwt_lifecycle::CLAIM_TENANT] = slug.into();
        self.sign_payload(&payload)
    }

    fn exp(&self) -> i64 {
        chrono::Utc::now().timestamp() + self.ttl_secs
    }

    fn sign_payload(&self, payload: &serde_json::Value) -> String {
        use base64::Engine;
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(payload).unwrap_or_default());
        let sig = hmac_sha256(&self.secret, payload_b64.as_bytes());
        let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
        format!("{payload_b64}.{sig_b64}")
    }

    /// Verify and return `(sub, jti, login session)`.
    ///
    /// Accepts both token shapes. Three segments is what `JwtLifecycle`
    /// issues since #1397 and what any standard JWT looks like; two is
    /// what `JwtBackend::issue` still emits and what `JwtLifecycle` emitted
    /// before it. Signing input differs — `header.payload` for the former,
    /// `payload` alone for the latter — so each is checked against its own.
    ///
    /// Handling both is not politeness: `docs/auth-jwt-api.md` tells you to
    /// pair this backend with `JwtLifecycle`, and between #1397 and this
    /// change that pairing silently stopped authenticating anyone, because
    /// the caller below required exactly one dot.
    ///
    /// **Refresh tokens are refused.** They carry `typ: "refresh"` and are
    /// wire-identical to access tokens, so without this check one
    /// authenticates as a bearer credential with the refresh token's much
    /// longer life — seven days by default (#1402).
    ///
    /// The tenant binding and `kind` go through [`UserTokenScope`] (#1848).
    ///
    /// [`UserTokenScope`]: super::jwt_lifecycle::UserTokenScope
    fn verify_claims(
        &self,
        token: &str,
        scope: super::jwt_lifecycle::UserTokenScope<'_>,
    ) -> Option<(
        i64,
        Option<String>,
        Option<super::auth_routes::RefreshSession>,
    )> {
        use base64::Engine;
        use subtle::ConstantTimeEq;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let parts: Vec<&str> = token.split('.').collect();
        let (signing_input, payload_b64, sig_b64) = match parts.as_slice() {
            [header, payload, sig] => {
                // Pin the algorithm. The HMAC below catches a swapped one
                // anyway, but refusing here makes `alg: none` fail as
                // itself rather than as a signature mismatch.
                let h: serde_json::Value =
                    serde_json::from_slice(&b64.decode(header).ok()?).ok()?;
                if h.get("alg").and_then(serde_json::Value::as_str) != Some("HS256") {
                    return None;
                }
                (format!("{header}.{payload}"), *payload, *sig)
            }
            [payload, sig] => ((*payload).to_owned(), *payload, *sig),
            _ => return None,
        };

        let expected = hmac_sha256(&self.secret, signing_input.as_bytes());
        let provided = b64.decode(sig_b64).ok()?;
        if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
            return None;
        }

        let payload: serde_json::Value =
            serde_json::from_slice(&b64.decode(payload_b64).ok()?).ok()?;
        let exp = payload.get("exp")?.as_i64()?;
        if chrono::Utc::now().timestamp() >= exp {
            return None; // expired
        }
        // Absent `typ` is a `JwtBackend::issue` token, which is an access
        // credential by construction. Present-and-not-"access" is refused.
        if let Some(typ) = payload.get("typ").and_then(serde_json::Value::as_str) {
            if typ != "access" {
                return None;
            }
        }
        let claims = payload.as_object()?;
        scope.admits(claims).ok()?;
        let sub = payload.get("sub")?.as_i64()?;
        let jti = payload
            .get("jti")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Some((sub, jti, super::auth_routes::RefreshSession::read(claims)))
    }
}

/// v0.38 — inline HMAC-SHA256(secret, msg) so JwtBackend signs
/// without depending on the PG-gated `operator_console::session::sign`.
/// Equivalent to the shared primitive — same `hmac` + `sha2` crates.
fn hmac_sha256(secret: &[u8], msg: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(msg);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    out
}

#[async_trait]
impl AuthBackend for JwtBackend {
    async fn authenticate(
        &self,
        parts: &Parts,
        pool: &Pool,
    ) -> Result<Option<AuthUser>, AuthError> {
        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;

        let bearer = extract_bearer(parts)?;
        let Some(token) = bearer else {
            return Ok(None);
        };

        // A JWT is two dots (`header.payload.sig`) since #1397, or one for
        // the legacy shape `JwtBackend::issue` still emits. An API key is
        // also one dot (`prefix.secret`), distinguished by its 8-char hex
        // prefix — a base64 JWT payload is never 8 characters.
        //
        // This used to require *exactly* one dot, which meant a real JWT
        // was handed back as "not mine" before verification ran. Between
        // #1397 and #1402 that silently stopped `JwtLifecycle` tokens
        // authenticating through the backend chain the docs tell you to
        // pair them with.
        let dots = token.chars().filter(|&c| c == '.').count();
        if dots != 1 && dots != 2 {
            return Ok(None);
        }
        if dots == 1 && token.split_once('.').map(|(p, _)| p.len()) == Some(8) {
            return Ok(None);
        }

        // All tenants share one key, so a resolved tenant must match the
        // token's `tenant` claim (#1848).
        let scope = match parts.extensions.get::<super::TenantSlug>() {
            Some(slug) => super::jwt_lifecycle::UserTokenScope::Tenant(&slug.0),
            None => super::jwt_lifecycle::UserTokenScope::Unscoped,
        };
        let (user_id, jti, session) = match self.verify_claims(token, scope) {
            Some(c) => c,
            None => return Err(AuthError::InvalidToken),
        };

        // Revocation, when a store is wired (#1402). Without this the
        // blacklist is write-only: `revoke()` and `/api/auth/logout` record
        // a JTI that nothing ever reads, so a "logged out" token keeps
        // working for its full remaining life.
        match (self.jti_store.as_ref(), jti.as_deref()) {
            (Some(store), Some(jti)) if store.is_used(jti).await => {
                return Err(AuthError::InvalidToken);
            }
            (None, Some(_)) => self.warn_unchecked_revocation(),
            _ => {}
        }

        let users = super::auth::User::objects()
            .where_(super::auth::User::id.eq(user_id))
            .fetch(pool)
            .await?;

        let Some(user) = users.into_iter().next() else {
            return Ok(None);
        };

        if !user.active {
            return Err(AuthError::Inactive);
        }

        // A login token ends with its session, as on `require_bearer` (#2247).
        if let Some(session) = session {
            let check = super::auth_routes::SessionCheck {
                pwf_secret: &self.pwf_secret,
                families: self.jti_store.as_deref(),
                cap_secs: None,
            };
            if !session.admits(&check, &user).await {
                return Err(AuthError::InvalidToken);
            }
        }

        Ok(Some(AuthUser {
            id: user.id.get().copied().unwrap_or(0),
            username: user.username,
            is_superuser: user.is_superuser,
        }))
    }
}

// ------------------------------------------------------------------ helpers

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn extract_bearer(parts: &Parts) -> Result<Option<&str>, AuthError> {
    let Some(value) = parts.headers.get(axum::http::header::AUTHORIZATION) else {
        return Ok(None);
    };
    let s = value.to_str().unwrap_or("");
    Ok(s.strip_prefix("Bearer ").map(str::trim))
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    /// Authenticate `token` twice on `backend`; count revocation warnings.
    async fn revocation_warnings(backend: &JwtBackend, token: &str) -> usize {
        let buf = crate::testkit::CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _g = tracing::subscriber::set_default(subscriber);
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        for _ in 0..2 {
            let (parts, ()) = axum::http::Request::builder()
                .header("authorization", format!("Bearer {token}"))
                .body(())
                .unwrap()
                .into_parts();
            let _ = backend.authenticate(&parts, &pool).await;
        }
        buf.contents().matches("not checked for revocation").count()
    }

    /// #1809 — a revocable token on a backend without a JTI store is not silent.
    #[tokio::test]
    async fn a_revocable_token_without_a_jti_store_warns_once() {
        let token = super::super::jwt_lifecycle::JwtLifecycle::new(vec![7u8; 32])
            .issue_pair(42)
            .access;
        let backend = JwtBackend::new(vec![7u8; 32]);
        assert_eq!(revocation_warnings(&backend, &token).await, 1);
    }

    /// A token with no `jti` can't be revoked, so there is nothing to warn about.
    #[tokio::test]
    async fn a_token_without_a_jti_stays_silent() {
        let backend = JwtBackend::new(vec![7u8; 32]);
        let token = backend.issue(42);
        assert_eq!(revocation_warnings(&backend, &token).await, 0);
    }
}
