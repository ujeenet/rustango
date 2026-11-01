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
//!         pool: &rustango::sql::Pool,
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
use crate::login_throttle::{ClientIp, LoginScope};

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
#[non_exhaustive]
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

fn client_ip(parts: &Parts) -> ClientIp {
    parts
        .extensions
        .get::<ClientIp>()
        .cloned()
        .unwrap_or_else(|| ClientIp::from_parts(&parts.extensions, &parts.headers))
}

fn tenant_slug(parts: &Parts) -> Result<&str, AuthError> {
    match parts.extensions.get::<super::TenantSlug>() {
        Some(slug) => Ok(&slug.0),
        None => {
            tracing::error!(
                "auth backend ran without a TenantSlug; use require_auth / optional_auth"
            );
            Err(AuthError::InvalidToken)
        }
    }
}

/// How long a verified Basic credential skips argon2.
const BASIC_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
/// Entries one tenant's Basic cache holds before it evicts; a busy tenant
/// only evicts its own entries.
const BASIC_CACHE_CAP: usize = 256;

type CacheKey = [u8; 32];

/// Verified Basic credentials, bucketed per tenant slug.
struct BasicCache {
    ttl: std::time::Duration,
    cap: usize,
    tenants: std::collections::HashMap<
        String,
        std::collections::HashMap<CacheKey, (i64, std::time::Instant)>,
    >,
}

impl BasicCache {
    fn new(ttl: std::time::Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            tenants: std::collections::HashMap::new(),
        }
    }

    fn hit(&self, slug: &str, key: &CacheKey, user_id: i64, now: std::time::Instant) -> bool {
        self.tenants
            .get(slug)
            .and_then(|m| m.get(key))
            .is_some_and(|(id, at)| *id == user_id && now.saturating_duration_since(*at) < self.ttl)
    }

    fn put(&mut self, slug: &str, key: CacheKey, user_id: i64, now: std::time::Instant) {
        let ttl = self.ttl;
        // Expired buckets of other tenants go too, so idle slugs don't pile up.
        self.tenants.retain(|s, m| {
            s == slug
                || m.values()
                    .any(|(_, at)| now.saturating_duration_since(*at) < ttl)
        });
        let map = self.tenants.entry(slug.to_owned()).or_default();
        if map.len() >= self.cap && !map.contains_key(&key) {
            map.retain(|_, (_, at)| now.saturating_duration_since(*at) < ttl);
            if map.len() >= self.cap {
                let oldest = map.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| *k);
                if let Some(oldest) = oldest {
                    map.remove(&oldest);
                }
            }
        }
        map.insert(key, (user_id, now));
    }
}

fn basic_cache() -> &'static std::sync::Mutex<BasicCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<BasicCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(BasicCache::new(BASIC_CACHE_TTL, BASIC_CACHE_CAP)))
}

/// SHA-256 over the tenant, the credential and the stored hash. A password
/// change rewrites the hash, so it misses; `active` is read per request.
fn basic_cache_key(slug: &str, username: &str, password: &str, stored_hash: &str) -> CacheKey {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    for part in [slug, username, password, stored_hash] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().into()
}

fn basic_cache_hit(slug: &str, key: &CacheKey, user_id: i64) -> bool {
    basic_cache()
        .lock()
        .is_ok_and(|c| c.hit(slug, key, user_id, std::time::Instant::now()))
}

fn basic_cache_put(slug: &str, key: CacheKey, user_id: i64) {
    if let Ok(mut cache) = basic_cache().lock() {
        cache.put(slug, key, user_id, std::time::Instant::now());
    }
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
        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());

        // `username` is the `[auth] login_field` value.
        let (username, password) = match parse_basic_auth(auth_header) {
            Some(pair) => pair,
            None => return Ok(None),
        };

        // Own lock scope, apart from the login forms; only failures count.
        // Sent on every request: admitted before the lookup, so a locked or
        // spent caller is refused first and a burst holds slots, not checks.
        let slug = tenant_slug(parts)?;
        let scope = LoginScope::TenantBasic(slug.to_owned());
        let refused = AuthError::Refused;
        let mut attempt = crate::login_throttle::shared()
            .begin_per_request(&scope, &client_ip(parts), &username)
            .await
            .map_err(refused)?;
        let field = super::login_field();
        let user = super::find_login_user(pool, field, &username).await?;

        // A credential verified moments ago against this same stored hash
        // skips argon2: it is not a guess.
        let cache_key = user
            .as_ref()
            .map(|u| basic_cache_key(slug, &username, &password, &u.password_hash));
        let cached = |u: &super::auth::User| {
            let uid = u.id.get().copied().unwrap_or(0);
            let key = cache_key.as_ref()?;
            (u.active && uid != 0 && basic_cache_hit(slug, key, uid)).then(|| AuthUser {
                id: uid,
                username: u.username.clone(),
                is_superuser: u.is_superuser,
            })
        };
        if let Some((u, hit)) = user.as_ref().and_then(|u| Some((u, cached(u)?))) {
            // The stored identifier's lock too, when the submitted one differs.
            attempt
                .resolve(super::login_key(field, u))
                .await
                .map_err(refused)?;
            attempt.settle(true).await;
            return Ok(Some(hit));
        }

        // A check at hash cost is reserved like a login.
        attempt.reserve().await.map_err(refused)?;
        let Some(user) = user else {
            // Audit H1/N4 — spend a verify's worth of work on the
            // unknown-user path so timing doesn't reveal whether the
            // username exists.
            password::verify_dummy_async_in(HashLane::Credential, &password)
                .await
                .map_err(AuthError::from_hash)?;
            attempt.settle(false).await;
            return Ok(None);
        };
        attempt
            .resolve(super::login_key(field, &user))
            .await
            .map_err(AuthError::Refused)?;
        // A parallel request may have verified it while this one waited.
        if let Some(hit) = cached(&user) {
            attempt.settle(true).await;
            return Ok(Some(hit));
        }

        // Verify before the active check so active vs inactive accounts
        // take the same time (audit H1/N4).
        let ok = password::verify_async_in(HashLane::Credential, &password, &user.password_hash)
            .await
            .map_err(AuthError::from_hash)?;
        // Only after a right password, so the refusal reads as a wrong one.
        // Its extra reads are not timing-hidden; the caller knew the password.
        let registry = super::middleware::session_keys(&parts.extensions).map(|k| k.registry);
        let ok = ok && super::auth::password_login_allowed(pool, registry.as_ref(), &user).await;
        if !user.active || !ok {
            attempt.settle(false).await;
            // Audit N4 — an inactive account must look identical to a
            // wrong password at this (username-keyed, pre-credential)
            // boundary: same `Ok(None)`, not a distinguishable
            // `Err(Inactive)`, so the backend can't be used to enumerate
            // accounts. (Inactive is still enforced after a *valid* API
            // key / JWT below, where the caller already proved ownership.)
            return Ok(None);
        }

        attempt.settle(true).await;
        let id = user.id.get().copied().unwrap_or(0);
        if let (Some(key), true) = (cache_key, id != 0) {
            basic_cache_put(slug, key, id);
        }
        Ok(Some(AuthUser {
            id,
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
#[derive(crate::Model, Clone)]
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
    /// but NOT the secret (only its keyed MAC is in `key_hash`), so a
    /// stolen prefix can't authenticate on its own.
    ///
    /// Indexed: every Bearer request looks the row up by this column,
    /// so without one it is a sequential scan on the hit path — 5.9 ms
    /// at 100k keys against 0.025 ms indexed, growing with every key
    /// ever issued (#1647).
    #[rustango(max_length = 8, index)]
    pub key_prefix: String,
    /// `hmac-sha256v2$<hex>` under a sub-key of the server secret;
    /// `hmac-sha256$` (the secret itself) still verifies; argon2id rows are
    /// rewritten on first use.
    /// Never returned to callers.
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

redacted_debug!(ApiKey { id, user_id, key_prefix, label, expires_at, created_at; key_hash });

// A key is minted by `create-api-key`, which shows the secret once; an admin
// form could only store a row nobody can use (#1763).
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_api_keys",
    "add",
    |_, _| false
);
// The owner decides who the credential signs in as (#2520).
#[cfg(feature = "admin")]
crate::register_admin_superuser_fields!(ApiKey, Add, [user_id]);
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
/// Generate a key with `cargo run -- create-api-key <slug> <username>`.
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
        // Sent on every request: admitted before the lookup, only a miss counts.
        let scope = LoginScope::TenantApiKey(tenant_slug(parts)?.to_owned());
        let mut attempt = crate::login_throttle::shared()
            .begin_per_request(&scope, &client_ip(parts), "")
            .await
            .map_err(AuthError::Refused)?;
        let server_key = api_key_secret().map_err(AuthError::from_hash)?;
        let mac = api_key_mac(server_key, secret);
        let keys = ApiKey::objects()
            .where_(ApiKey::key_prefix.eq(prefix.to_owned()))
            .fetch(pool)
            .await?;
        // The prefix is random, not unique: try every row (#2250).
        let (mut macs, mut raw_macs, mut legacy) = (Vec::new(), Vec::new(), Vec::new());
        for k in keys {
            match StoredKeyHash::of(&k.key_hash) {
                StoredKeyHash::Mac => macs.push(k),
                StoredKeyHash::RawSecretMac => raw_macs.push(k),
                StoredKeyHash::Argon2 => legacy.push(k),
            }
        }
        let raw_hit = || {
            let raw = raw_secret_api_key_mac(server_key, secret);
            raw_macs.into_iter().find(|k| mac_eq(&k.key_hash, &raw))
        };
        let key = match macs.into_iter().find(|k| mac_eq(&k.key_hash, &mac)) {
            Some(k) => Some(k),
            // Keyed by the secret itself; left as is so a rollback still verifies.
            None => match raw_hit() {
                Some(k) => Some(k),
                None if legacy.is_empty() => None,
                // Older argon2 rows: verify once, then store the MAC.
                // A guess at hash cost is reserved like a login.
                None => {
                    attempt.reserve().await.map_err(AuthError::Refused)?;
                    let key = password::first_verified(legacy, secret, |k| &k.key_hash)
                        .await
                        .map_err(AuthError::from_hash)?;
                    if let Some(k) = &key {
                        upgrade_api_key(pool, k, &mac).await;
                    }
                    key
                }
            },
        };
        attempt.settle(key.is_some()).await;
        let Some(key) = key else {
            return Ok(None);
        };

        // Verify before judging expiry, so an expired prefix costs the
        // same as an unknown one (#1729).
        if key.expires_at.is_some_and(|exp| chrono::Utc::now() > exp) {
            return Err(AuthError::InvalidToken);
        }

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

/// `key_hash` prefix of a key MACed with the derived sub-key.
const API_KEY_MAC_SCHEME: &str = "hmac-sha256v2$";
/// Prefix of a key MACed with the server secret itself (0.60.x, 0.61.0-1).
const API_KEY_RAW_MAC_SCHEME: &str = "hmac-sha256$";
/// Domain tag, so a key MAC is never a valid MAC of anything else.
const API_KEY_MAC_TAG: &[u8] = b"rustango-apikey-v1.";
/// KDF label: the API-key sub-key is HMAC(server secret, label).
const API_KEY_KDF_LABEL: &[u8] = b"rustango-apikey-mac-key-v2";

/// How a stored `key_hash` is checked.
enum StoredKeyHash {
    Mac,
    RawSecretMac,
    Argon2,
}

impl StoredKeyHash {
    fn of(key_hash: &str) -> Self {
        if key_hash.starts_with(API_KEY_MAC_SCHEME) {
            Self::Mac
        } else if key_hash.starts_with(API_KEY_RAW_MAC_SCHEME) {
            Self::RawSecretMac
        } else {
            Self::Argon2
        }
    }
}

static API_KEY_SECRET: std::sync::OnceLock<crate::session::SessionSecret> =
    std::sync::OnceLock::new();

/// Set the key that MACs API keys. The first call wins; returns `false` after.
/// Unset, it is `RUSTANGO_SESSION_SECRET`, or in dev the tenant session key file.
/// Changing this key (or rotating that secret) invalidates every API key.
pub fn set_api_key_secret(secret: crate::session::SessionSecret) -> bool {
    API_KEY_SECRET.set(secret).is_ok()
}

/// The CLI that mints a key and the server that checks it must agree, so
/// this reads the env or the dev key file, never a per-process random key.
fn api_key_secret() -> Result<&'static crate::session::SessionSecret, super::TenancyError> {
    if let Some(s) = API_KEY_SECRET.get() {
        return Ok(s);
    }
    let secret = super::session::tenant_secret_from_env().inspect_err(|e| {
        tracing::error!(error = %e, "API keys need RUSTANGO_SESSION_SECRET on the prod tier");
    })?;
    Ok(API_KEY_SECRET.get_or_init(|| secret))
}

/// The CLI's key: a set one, else the server's, loaded only, never written.
pub(crate) fn existing_api_key_secret() -> Result<
    (
        &'static crate::session::SessionSecret,
        Option<super::session::KeySource>,
    ),
    super::TenancyError,
> {
    if let Some(s) = API_KEY_SECRET.get() {
        return Ok((s, None));
    }
    let (secret, source) =
        super::session::existing_tenant_secret(super::session::DevKeyFile::LoadOnly)?;
    Ok((API_KEY_SECRET.get_or_init(|| secret), Some(source)))
}

/// `hmac-sha256v2$<hex>` of the secret half under a sub-key derived from
/// `key`, so the server secret never keys this MAC; the prefix is only the lookup.
fn api_key_mac(key: &crate::session::SessionSecret, secret: &str) -> String {
    let sub = crate::session::sign(key, API_KEY_KDF_LABEL).to_vec();
    tagged_mac(
        API_KEY_MAC_SCHEME,
        &crate::session::SessionSecret::from_bytes(sub),
        secret,
    )
}

/// The 0.60.x MAC, keyed by the server secret itself; verified, never minted.
fn raw_secret_api_key_mac(key: &crate::session::SessionSecret, secret: &str) -> String {
    tagged_mac(API_KEY_RAW_MAC_SCHEME, key, secret)
}

fn tagged_mac(scheme: &str, key: &crate::session::SessionSecret, secret: &str) -> String {
    let msg = [API_KEY_MAC_TAG, secret.as_bytes()].concat();
    format!(
        "{scheme}{}",
        crate::hex::hex_encode(&crate::session::sign(key, &msg))
    )
}

fn mac_eq(stored: &str, mac: &str) -> bool {
    crate::crypto::constant_time_compare(stored.as_bytes(), mac.as_bytes())
}

/// Swap a verified argon2 hash for the MAC. A failure only costs one more
/// argon2 next time, so it is logged, not raised.
async fn upgrade_api_key(pool: &Pool, key: &ApiKey, mac: &str) {
    use crate::core::Column as _;
    use crate::sql::UpdaterPool as _;
    let Some(id) = key.id.get().copied() else {
        return;
    };
    if let Err(e) = ApiKey::objects()
        .where_(ApiKey::id.eq(id))
        .update()
        .set("key_hash", mac.to_owned())
        .execute_pool(pool)
        .await
    {
        tracing::warn!(error = %e, key_id = id, "could not upgrade an argon2 API key");
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
    create_api_key_signed(api_key_secret()?, user_id, label, expires_at, pool).await
}

/// [`create_api_key`] with the MAC key given, so the CLI can pass a loaded one.
pub(crate) async fn create_api_key_signed(
    mac_key: &crate::session::SessionSecret,
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
    let prefix = crate::hex::hex_encode(&prefix_bytes);
    let mut secret_bytes = [0u8; 16];
    OsRng.fill_bytes(&mut secret_bytes);
    let secret = crate::hex::hex_encode(&secret_bytes);

    let hash = api_key_mac(mac_key, &secret);

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
    /// Set once this backend has warned about a token with no `iat`.
    warned_legacy: std::sync::atomic::AtomicBool,
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
            warned_legacy: std::sync::atomic::AtomicBool::new(false),
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

    /// A token from before `iat` was stamped: it cannot be tied to a session.
    fn warn_legacy_token(&self) {
        use std::sync::atomic::Ordering;
        if !self.warned_legacy.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "rustango::tenancy",
                "JwtBackend accepted a token with no `iat`: it survives logout-all and \
                 password changes until it expires; reissue it"
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
    /// Logout-all and a password change end it. It does not know the user's
    /// logout stamp, so one minted in the second after a logout-all is
    /// refused: right after a logout-all, use [`Self::issue_for_user`].
    #[must_use]
    pub fn issue(&self, user_id: i64) -> String {
        self.sign_payload(&self.claims(user_id, None))
    }

    /// [`Self::issue`] bound to tenant `slug`, for `require_auth` routes (#1848).
    /// Right after a logout-all, use [`Self::issue_for_tenant_user`].
    #[must_use]
    pub fn issue_for_tenant(&self, user_id: i64, slug: &str) -> String {
        self.sign_payload(&self.claims(user_id, Some(slug)))
    }

    /// [`Self::issue`] for `user`, also ended when its password hash changes.
    /// Valid even when minted in the same second as a logout-all.
    #[must_use]
    pub fn issue_for_user(&self, user: &super::auth::User) -> String {
        self.sign_payload(&self.user_claims(user, None))
    }

    /// [`Self::issue_for_tenant`] for `user`, also ended when its password hash changes.
    #[must_use]
    pub fn issue_for_tenant_user(&self, user: &super::auth::User, slug: &str) -> String {
        self.sign_payload(&self.user_claims(user, Some(slug)))
    }

    fn claims(&self, user_id: i64, tenant: Option<&str>) -> serde_json::Value {
        let now = chrono::Utc::now().timestamp();
        let mut payload = serde_json::json!({"sub": user_id, "exp": self.exp(), "iat": now});
        // Only a store can revoke a jti; without one it would only warn.
        if self.jti_store.is_some() {
            payload["jti"] = super::jwt_lifecycle::random_jti().into();
        }
        if let Some(slug) = tenant {
            payload[super::jwt_lifecycle::CLAIM_TENANT] = slug.into();
        }
        payload
    }

    fn user_claims(&self, user: &super::auth::User, tenant: Option<&str>) -> serde_json::Value {
        let mut payload = self.claims(user.id.get().copied().unwrap_or(0), tenant);
        // Past the logout stamp, like the session claims below.
        payload["iat"] = crate::session::issued_at(user.sessions_revoked_at).into();
        if let Some(claims) = payload.as_object_mut() {
            super::auth_routes::RefreshSession::for_user(&self.pwf_secret, user).write(claims);
        }
        payload
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
    ) -> Option<VerifiedClaims> {
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
        Some(VerifiedClaims {
            sub,
            jti,
            iat: payload.get("iat").and_then(serde_json::Value::as_i64),
            session: super::auth_routes::RefreshSession::read(claims),
        })
    }
}

/// What [`JwtBackend::verify_claims`] read from a valid token.
struct VerifiedClaims {
    sub: i64,
    jti: Option<String>,
    iat: Option<i64>,
    /// The login-session claims, when the token carries them.
    session: Option<super::auth_routes::RefreshSession>,
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
        let Some(VerifiedClaims {
            sub: user_id,
            jti,
            iat,
            session,
        }) = self.verify_claims(token, scope)
        else {
            return Err(AuthError::InvalidToken);
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
        // Without session claims, `iat` still meets logout-all and password changes.
        let admitted = match (session, iat) {
            (Some(session), _) => {
                let check = super::auth_routes::SessionCheck {
                    pwf_secret: &self.pwf_secret,
                    families: self.jti_store.as_deref(),
                    cap_secs: None,
                };
                session.admits(&check, &user).await
            }
            (None, Some(iat)) => {
                user.password_changed_at
                    .is_none_or(|ts| iat >= ts.timestamp())
                    && user
                        .sessions_revoked_at
                        .is_none_or(|ts| iat > ts.timestamp())
            }
            (None, None) => {
                self.warn_legacy_token();
                true
            }
        };
        if !admitted {
            return Err(AuthError::InvalidToken);
        }

        Ok(Some(AuthUser {
            id: user.id.get().copied().unwrap_or(0),
            username: user.username,
            is_superuser: user.is_superuser,
        }))
    }
}

// ------------------------------------------------------------------ helpers

fn extract_bearer(parts: &Parts) -> Result<Option<&str>, AuthError> {
    Ok(crate::bearer::token(&parts.headers))
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    /// The MAC depends on the server key and the secret.
    #[test]
    fn api_key_mac_binds_key_and_secret() {
        use crate::session::SessionSecret;
        let (a, b) = (
            SessionSecret::from_bytes(vec![1; 32]),
            SessionSecret::from_bytes(vec![2; 32]),
        );
        let mac = api_key_mac(&a, "s3cret");
        assert!(mac.starts_with(API_KEY_MAC_SCHEME) && mac.len() < 255);
        assert!(mac_eq(&mac, &api_key_mac(&a, "s3cret")));
        assert_ne!(mac, api_key_mac(&b, "s3cret"));
        assert_ne!(mac, api_key_mac(&a, "s3creu"));
        // Keyed by the derived sub-key, not the secret itself.
        let raw = raw_secret_api_key_mac(&a, "s3cret");
        assert_ne!(
            &raw[API_KEY_RAW_MAC_SCHEME.len()..],
            &mac[API_KEY_MAC_SCHEME.len()..]
        );
    }

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

    /// The Basic cache key changes with the stored hash and the
    /// tenant, so a password change or another tenant never hits.
    #[test]
    fn basic_cache_key_binds_hash_and_tenant() {
        let k = basic_cache_key("t", "alice", "pw", "$argon2id$a");
        assert_eq!(k, basic_cache_key("t", "alice", "pw", "$argon2id$a"));
        assert_ne!(k, basic_cache_key("t", "alice", "pw", "$argon2id$b"));
        assert_ne!(k, basic_cache_key("u", "alice", "pw", "$argon2id$a"));
        assert_ne!(k, basic_cache_key("t", "alice", "pw2", "$argon2id$a"));
        assert_ne!(
            basic_cache_key("t", "ab", "c", "h"),
            basic_cache_key("t", "a", "bc", "h")
        );
    }

    #[test]
    fn basic_cache_hit_needs_the_same_user() {
        let key = basic_cache_key("t-hit", "bob", "pw", "h");
        assert!(!basic_cache_hit("t-hit", &key, 7));
        basic_cache_put("t-hit", key, 7);
        assert!(basic_cache_hit("t-hit", &key, 7));
        assert!(!basic_cache_hit("t-hit", &key, 8));
    }

    fn key_n(n: u32) -> CacheKey {
        let mut k = [0u8; 32];
        k[..4].copy_from_slice(&n.to_le_bytes());
        k
    }

    #[test]
    fn basic_cache_entry_expires_after_ttl() {
        let ttl = std::time::Duration::from_secs(30);
        let mut cache = BasicCache::new(ttl, 4);
        let t0 = std::time::Instant::now();
        cache.put("a", key_n(1), 1, t0);
        assert!(cache.hit(
            "a",
            &key_n(1),
            1,
            t0 + ttl - std::time::Duration::from_millis(1)
        ));
        assert!(!cache.hit("a", &key_n(1), 1, t0 + ttl));
    }

    #[test]
    fn basic_cache_full_tenant_evicts_its_oldest_entry() {
        let mut cache = BasicCache::new(std::time::Duration::from_secs(30), 2);
        let t0 = std::time::Instant::now();
        let s = std::time::Duration::from_secs(1);
        cache.put("a", key_n(1), 1, t0);
        cache.put("a", key_n(2), 2, t0 + s);
        cache.put("a", key_n(3), 3, t0 + 2 * s);
        let now = t0 + 3 * s;
        assert!(!cache.hit("a", &key_n(1), 1, now));
        assert!(cache.hit("a", &key_n(2), 2, now));
        assert!(cache.hit("a", &key_n(3), 3, now));
    }

    #[test]
    fn basic_cache_busy_tenant_never_evicts_another() {
        let mut cache = BasicCache::new(std::time::Duration::from_secs(30), 2);
        let t0 = std::time::Instant::now();
        cache.put("quiet", key_n(0), 9, t0);
        for n in 1..50 {
            cache.put(
                "busy",
                key_n(n),
                i64::from(n),
                t0 + std::time::Duration::from_millis(u64::from(n)),
            );
        }
        let now = t0 + std::time::Duration::from_secs(1);
        assert!(cache.hit("quiet", &key_n(0), 9, now));
        assert!(
            !cache.hit("busy", &key_n(0), 9, now),
            "buckets are per tenant"
        );
        assert_eq!(cache.tenants["busy"].len(), 2);
    }

    #[test]
    fn basic_cache_drops_expired_tenant_buckets() {
        let ttl = std::time::Duration::from_secs(30);
        let mut cache = BasicCache::new(ttl, 2);
        let t0 = std::time::Instant::now();
        cache.put("old", key_n(1), 1, t0);
        cache.put("new", key_n(2), 2, t0 + ttl);
        assert!(!cache.tenants.contains_key("old"));
    }

    /// performance-001 / security-003 — a locked Basic account is refused
    /// before the user lookup: here there is no user table to look in.
    #[tokio::test]
    async fn a_locked_basic_account_is_refused_before_the_lookup() {
        use crate::login_throttle::LoginRefused;
        use base64::Engine as _;
        let scope = LoginScope::TenantBasic("t-prelock".into());
        for _ in 0..crate::account_lockout::DEFAULT_MAX_ATTEMPTS {
            let a = crate::login_throttle::shared()
                .begin(&scope, &ClientIp::default(), "locked-user")
                .await
                .unwrap();
            a.failed().await;
        }
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        let creds = base64::engine::general_purpose::STANDARD.encode("locked-user:pw");
        let (parts, ()) = axum::http::Request::builder()
            .header("authorization", format!("Basic {creds}"))
            .extension(super::super::TenantSlug("t-prelock".into()))
            .body(())
            .unwrap()
            .into_parts();
        let r = ModelBackend.authenticate(&parts, &pool).await;
        assert!(
            matches!(r, Err(AuthError::Refused(LoginRefused::Throttled { .. }))),
            "{r:?}"
        );
    }

    /// Parallel wrong keys through a slow shared cache spend the per-IP
    /// limit, and once it is spent the right key is refused.
    #[tokio::test]
    async fn a_parallel_burst_of_api_key_guesses_is_bounded_by_the_ip_limit() {
        let slow = std::sync::Arc::new(
            crate::login_throttle::counting::CountingWrites::with_read_delay(
                std::time::Duration::from_millis(100),
            ),
        );
        // A window that cannot roll over mid-burst: the per-IP windows line
        // up with the clock, and the slow cache stretches the burst.
        let limits = crate::login_throttle::LoginLimits {
            ip_limit: 3,
            ip_window: std::time::Duration::from_secs(10 * 365 * 24 * 3600),
            ..Default::default()
        };
        let installed = crate::login_throttle::configure_shared(
            crate::login_throttle::LoginThrottle::with_cache(limits, slow),
        );
        // nextest runs each test in its own process; another gate won here.
        if !installed {
            return;
        }
        set_api_key_secret(crate::session::SessionSecret::from_bytes(vec![7; 32]));
        let (pool, user) = jwt_user("api_key_burst_gate").await;
        crate::testkit::create_tables_for::<ApiKey>(&pool)
            .await
            .unwrap();
        let token = create_api_key(*user.id.get().unwrap(), "t", None, &pool)
            .await
            .unwrap();
        let (prefix, _) = token.split_once('.').unwrap();
        let call = |token: String| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let (parts, ()) = axum::http::Request::builder()
                    .header("authorization", format!("Bearer {token}"))
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [10, 64, 0, 2],
                        1,
                    ))))
                    .extension(super::super::TenantSlug("t-burst-key".into()))
                    .body(())
                    .unwrap()
                    .into_parts();
                ApiKeyBackend.authenticate(&parts, &pool).await
            })
        };
        let guesses: Vec<_> = (0..30)
            .map(|i| call(format!("{prefix}.guess{i}")))
            .collect();
        // The right one arrives while the wrong ones are in flight.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let right = call(token.clone());
        let mut misses = 0;
        for g in guesses {
            match g.await.unwrap() {
                Ok(None) => misses += 1,
                Err(AuthError::Refused(_)) => {}
                other => panic!("a wrong key got {:?}", other.map(|u| u.map(|u| u.id))),
            }
        }
        assert!(misses <= 3, "{misses} wrong keys were checked");
        // In flight, the right one may pass before the limit is spent.
        match right.await.unwrap() {
            Ok(Some(_)) | Err(AuthError::Refused(_)) => {}
            other => panic!("the right key got {:?}", other.map(|u| u.map(|u| u.id))),
        }
        // Spend what is left, even past a window edge.
        let mut more = 0;
        while matches!(call(format!("{prefix}.late")).await.unwrap(), Ok(None)) {
            more += 1;
            assert!(more <= 3, "{more} more wrong keys were checked");
        }
        assert!(
            matches!(
                call(token.clone()).await.unwrap(),
                Err(AuthError::Refused(_))
            ),
            "the right key passed a spent per-IP limit"
        );
    }

    /// Parallel Basic guesses through a slow shared cache, from a client
    /// that signed in moments ago: the account lock still bounds the burst,
    /// and once it locks the right password is refused, verified cache or not.
    #[tokio::test]
    async fn a_parallel_burst_of_basic_user_guesses_is_bounded_by_the_lock() {
        use base64::Engine as _;
        let slow = std::sync::Arc::new(
            crate::login_throttle::counting::CountingWrites::with_read_delay(
                std::time::Duration::from_millis(100),
            ),
        );
        let installed = crate::login_throttle::configure_shared(
            crate::login_throttle::LoginThrottle::with_cache(
                crate::login_throttle::LoginLimits::default(),
                slow,
            ),
        );
        // nextest runs each test in its own process; another gate won here.
        if !installed {
            return;
        }
        let (pool, mut user) = jwt_user("basic_burst_gate").await;
        user.password_hash = password::hash("s3cret-basic").unwrap();
        user.save_pool(&pool).await.unwrap();
        let call = |pass: String| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let creds =
                    base64::engine::general_purpose::STANDARD.encode(format!("alice:{pass}"));
                let (parts, ()) = axum::http::Request::builder()
                    .header("authorization", format!("Basic {creds}"))
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [10, 66, 0, 1],
                        1,
                    ))))
                    .extension(super::super::TenantSlug("t-burst-basic".into()))
                    .body(())
                    .unwrap()
                    .into_parts();
                ModelBackend.authenticate(&parts, &pool).await
            })
        };
        // Known client, and the credential sits in the verified cache.
        assert!(matches!(
            call("s3cret-basic".into()).await.unwrap(),
            Ok(Some(_))
        ));
        let guesses: Vec<_> = (0..99).map(|i| call(format!("guess{i}"))).collect();
        // The right one arrives while the wrong ones are in flight.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let right = call("s3cret-basic".into());
        let mut misses = 0;
        for g in guesses {
            match g.await.unwrap() {
                Ok(None) => misses += 1,
                Err(AuthError::Refused(_)) => {}
                other => panic!("a wrong password got {:?}", other.map(|u| u.map(|u| u.id))),
            }
        }
        let max = crate::account_lockout::DEFAULT_MAX_ATTEMPTS as usize;
        assert!(misses <= max, "{misses} wrong passwords were checked");
        // In flight, the right one may pass before the lock; it clears nothing.
        match right.await.unwrap() {
            Ok(Some(_)) | Err(AuthError::Refused(_)) => {}
            other => panic!(
                "the right password got {:?}",
                other.map(|u| u.map(|u| u.id))
            ),
        }
        let mut more = 0;
        while matches!(call("late".into()).await.unwrap(), Ok(None)) {
            more += 1;
            assert!(misses + more <= max, "the failures were cleared");
        }
        assert!(
            matches!(
                call("s3cret-basic".into()).await.unwrap(),
                Err(AuthError::Refused(_))
            ),
            "the right password passed a locked account"
        );
    }

    /// A right API key is sent on every request: its slot is given back,
    /// so it leaves the per-IP limit as it was; a wrong one is counted.
    #[tokio::test]
    async fn right_api_keys_leave_the_limit_as_it_was() {
        let limits = crate::login_throttle::LoginLimits {
            ip_limit: 3,
            ..Default::default()
        };
        let cache: crate::cache::BoxedCache =
            std::sync::Arc::new(crate::cache::InMemoryCache::new());
        let installed = crate::login_throttle::configure_shared(
            crate::login_throttle::LoginThrottle::with_cache(limits, cache),
        );
        // nextest runs each test in its own process; another gate won here.
        if !installed {
            return;
        }
        set_api_key_secret(crate::session::SessionSecret::from_bytes(vec![7; 32]));
        let (pool, user) = jwt_user("api_key_quiet_gate").await;
        crate::testkit::create_tables_for::<ApiKey>(&pool)
            .await
            .unwrap();
        let token = create_api_key(*user.id.get().unwrap(), "t", None, &pool)
            .await
            .unwrap();
        let call = |token: String| {
            let pool = pool.clone();
            async move {
                let (parts, ()) = axum::http::Request::builder()
                    .header("authorization", format!("Bearer {token}"))
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [10, 64, 0, 1],
                        1,
                    ))))
                    .extension(super::super::TenantSlug("t-quiet-key".into()))
                    .body(())
                    .unwrap()
                    .into_parts();
                ApiKeyBackend.authenticate(&parts, &pool).await
            }
        };
        for _ in 0..10 {
            assert!(matches!(call(token.clone()).await, Ok(Some(_))));
        }
        let (prefix, _) = token.split_once('.').unwrap();
        for _ in 0..3 {
            assert!(matches!(call(format!("{prefix}.wrong")).await, Ok(None)));
        }
        assert!(
            matches!(call(token.clone()).await, Err(AuthError::Refused(_))),
            "wrong keys were not counted"
        );
    }

    /// A user row in a fresh shared in-memory `SQLite`.
    async fn jwt_user(name: &str) -> (Pool, crate::tenancy::auth::User) {
        let pool = Pool::connect(&format!("sqlite:file:{name}?mode=memory&cache=shared"))
            .await
            .unwrap();
        crate::testkit::create_tables_for::<crate::tenancy::auth::User>(&pool)
            .await
            .unwrap();
        let mut user = crate::testkit::user();
        user.insert_pool(&pool).await.unwrap();
        (pool, user)
    }

    async fn bearer(
        backend: &JwtBackend,
        token: &str,
        tenant: Option<&str>,
        pool: &Pool,
    ) -> Result<Option<AuthUser>, AuthError> {
        let mut req =
            axum::http::Request::builder().header("authorization", format!("Bearer {token}"));
        if let Some(slug) = tenant {
            req = req.extension(super::super::TenantSlug(slug.into()));
        }
        let (parts, ()) = req.body(()).unwrap().into_parts();
        backend.authenticate(&parts, pool).await
    }

    /// `issue` / `issue_for_tenant` tokens end on logout-all and on a password change.
    #[tokio::test]
    async fn issued_tokens_end_with_the_session() {
        let (pool, mut user) = jwt_user("jwt_issue_ends_with_session").await;
        let id = *user.id.get().unwrap();
        let backend = JwtBackend::new(vec![7u8; 32]);
        let plain = backend.issue(id);
        let bound = backend.issue_for_tenant(id, "acme");
        assert!(bearer(&backend, &plain, None, &pool)
            .await
            .unwrap()
            .is_some());
        assert!(bearer(&backend, &bound, Some("acme"), &pool)
            .await
            .unwrap()
            .is_some());
        let later = chrono::Utc::now() + chrono::Duration::seconds(5);
        for stamp in ["sessions_revoked_at", "password_changed_at"] {
            user.sessions_revoked_at = (stamp == "sessions_revoked_at").then_some(later);
            user.password_changed_at = (stamp == "password_changed_at").then_some(later);
            user.save_pool(&pool).await.unwrap();
            let r = bearer(&backend, &plain, None, &pool).await;
            assert!(matches!(r, Err(AuthError::InvalidToken)), "{stamp}: {r:?}");
            let r = bearer(&backend, &bound, Some("acme"), &pool).await;
            assert!(matches!(r, Err(AuthError::InvalidToken)), "{stamp}: {r:?}");
        }
    }

    /// A user token minted in the second a logout-all stamped is accepted.
    #[tokio::test]
    async fn a_user_token_right_after_logout_all_is_accepted() {
        let (pool, mut user) = jwt_user("jwt_after_logout_all").await;
        let cutoff = crate::session::logout_cutoff_at(None, chrono::Utc::now().timestamp());
        user.sessions_revoked_at = Some(cutoff);
        user.save_pool(&pool).await.unwrap();
        let backend = JwtBackend::new(vec![7u8; 32]);
        for (token, tenant) in [
            (backend.issue_for_user(&user), None),
            (backend.issue_for_tenant_user(&user, "acme"), Some("acme")),
        ] {
            let r = bearer(&backend, &token, tenant, &pool).await;
            assert!(matches!(r, Ok(Some(_))), "{tenant:?}: {r:?}");
        }
    }

    /// A user-bound token also ends when the password hash changes.
    #[tokio::test]
    async fn user_bound_tokens_end_with_the_password_hash() {
        let (pool, mut user) = jwt_user("jwt_user_bound_pwf").await;
        let backend = JwtBackend::new(vec![7u8; 32]);
        let token = backend.issue_for_tenant_user(&user, "acme");
        assert!(bearer(&backend, &token, Some("acme"), &pool)
            .await
            .unwrap()
            .is_some());
        user.password_hash = "$argon2id$v=19$m=19456,t=2,p=1$new$hash".into();
        user.save_pool(&pool).await.unwrap();
        let r = bearer(&backend, &token, Some("acme"), &pool).await;
        assert!(matches!(r, Err(AuthError::InvalidToken)), "{r:?}");
    }

    /// A token minted before these claims still works until it expires, with one warning.
    #[tokio::test]
    async fn a_legacy_token_is_accepted_with_one_warning() {
        let (pool, user) = jwt_user("jwt_legacy_token_warns").await;
        let backend = JwtBackend::new(vec![7u8; 32]);
        let legacy = backend.sign_payload(&serde_json::json!({
            "sub": *user.id.get().unwrap(),
            "exp": backend.exp(),
        }));
        let buf = crate::testkit::CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _g = tracing::subscriber::set_default(subscriber);
        for _ in 0..2 {
            assert!(bearer(&backend, &legacy, None, &pool)
                .await
                .unwrap()
                .is_some());
        }
        assert_eq!(
            buf.contents().matches("no `iat`").count(),
            1,
            "{}",
            buf.contents()
        );
    }

    /// A token with no `jti` can't be revoked, so there is nothing to warn about.
    #[tokio::test]
    async fn a_token_without_a_jti_stays_silent() {
        let backend = JwtBackend::new(vec![7u8; 32]);
        let token = backend.issue(42);
        assert_eq!(revocation_warnings(&backend, &token).await, 0);
    }
}
