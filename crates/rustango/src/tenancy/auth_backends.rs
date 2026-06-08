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
/// This is the default backend — equivalent to Django's `ModelBackend`.
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

        let users = super::auth::User::objects()
            .where_(super::auth::User::username.eq(username.clone()))
            .fetch_pool(pool)
            .await?;

        let Some(user) = users.into_iter().next() else {
            return Ok(None);
        };

        if !user.active {
            return Err(AuthError::Inactive);
        }

        let ok = password::verify(&password, &user.password_hash)
            .map_err(|_| AuthError::InvalidToken)?;
        if !ok {
            return Ok(None);
        }

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
    #[rustango(max_length = 8)]
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

const API_KEY_ENSURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_api_keys" (
    "id"         BIGSERIAL    PRIMARY KEY,
    "user_id"    BIGINT       NOT NULL
                               REFERENCES "rustango_users"("id")
                               ON DELETE CASCADE,
    "key_prefix" VARCHAR(8)   NOT NULL,
    "key_hash"   VARCHAR(255) NOT NULL,
    "label"      VARCHAR(100) NOT NULL DEFAULT '',
    "expires_at" TIMESTAMPTZ,
    "created_at" TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    CONSTRAINT "rustango_api_keys_prefix_uq" UNIQUE ("key_prefix")
);
"#;

/// v0.38 — SQLite counterpart of [`API_KEY_ENSURE_SQL`]. Same column
/// shape, SQLite affinities. `NOW()` → `CURRENT_TIMESTAMP`.
const API_KEY_ENSURE_SQL_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_api_keys" (
    "id"         INTEGER PRIMARY KEY AUTOINCREMENT,
    "user_id"    INTEGER NOT NULL
                          REFERENCES "rustango_users"("id")
                          ON DELETE CASCADE,
    "key_prefix" TEXT    NOT NULL,
    "key_hash"   TEXT    NOT NULL,
    "label"      TEXT    NOT NULL DEFAULT '',
    "expires_at" TEXT,
    "created_at" TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "rustango_api_keys_prefix_uq" UNIQUE ("key_prefix")
);
"#;

/// v0.38 — MySQL counterpart of [`API_KEY_ENSURE_SQL`]. Backticks +
/// `BIGINT AUTO_INCREMENT PRIMARY KEY` + `DATETIME(6)` for the
/// timestamp columns + `CURRENT_TIMESTAMP(6)` default.
const API_KEY_ENSURE_SQL_MYSQL: &str = r#"
CREATE TABLE IF NOT EXISTS `rustango_api_keys` (
    `id`         BIGINT AUTO_INCREMENT PRIMARY KEY,
    `user_id`    BIGINT       NOT NULL,
    `key_prefix` VARCHAR(8)   NOT NULL,
    `key_hash`   VARCHAR(255) NOT NULL,
    `label`      VARCHAR(100) NOT NULL DEFAULT '',
    `expires_at` DATETIME(6),
    `created_at` DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    CONSTRAINT `rustango_api_keys_prefix_uq` UNIQUE (`key_prefix`),
    CONSTRAINT `rustango_api_keys_fk_user`
        FOREIGN KEY (`user_id`) REFERENCES `rustango_users`(`id`) ON DELETE CASCADE
);
"#;

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
    let ddl = match pool.dialect().name() {
        "sqlite" => API_KEY_ENSURE_SQL_SQLITE,
        "mysql" => API_KEY_ENSURE_SQL_MYSQL,
        _ => API_KEY_ENSURE_SQL,
    };
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        crate::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .map_err(|e| match e {
                crate::sql::ExecError::Driver(err) => err,
                other => sqlx::Error::Protocol(format!("{other}")),
            })?;
    }
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

        // v0.38 — replaced the hand-rolled JOIN with two ORM round-
        // trips because tri-dialect sqlx doesn't expose a portable
        // raw-row decode path (PgRow/MySqlRow/SqliteRow are distinct
        // types). One round-trip per ApiKey lookup + one per user
        // resolve; both indexed (key_prefix UNIQUE + id PK) so the
        // total latency on the hot path is two index seeks.
        let keys = ApiKey::objects()
            .where_(ApiKey::key_prefix.eq(prefix.to_owned()))
            .fetch_pool(pool)
            .await?;
        let Some(key) = keys.into_iter().next() else {
            return Ok(None);
        };

        if let Some(exp) = key.expires_at {
            if chrono::Utc::now() > exp {
                return Err(AuthError::InvalidToken);
            }
        }

        let ok = password::verify(secret, &key.key_hash).map_err(|_| AuthError::InvalidToken)?;
        if !ok {
            return Ok(None);
        }

        let users = super::auth::User::objects()
            .where_(super::auth::User::id.eq(key.user_id))
            .fetch_pool(pool)
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

    let hash = password::hash(&secret)
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
    /// Token lifetime in seconds for tokens issued via [`JwtBackend::issue`].
    pub ttl_secs: i64,
}

impl JwtBackend {
    /// Build a backend from raw key bytes.
    #[must_use]
    pub fn new(secret: Vec<u8>) -> Self {
        Self {
            secret,
            ttl_secs: 3600,
        }
    }

    /// Build from the operator-console session secret (convenient for
    /// projects that don't want a separate signing key).
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn from_session_secret(s: &super::operator_console::SessionSecret) -> Self {
        Self::new(s.key().to_vec())
    }

    /// Issue a signed JWT for `user_id` valid for `self.ttl_secs`.
    #[must_use]
    pub fn issue(&self, user_id: i64) -> String {
        use base64::Engine;
        let exp = chrono::Utc::now().timestamp() + self.ttl_secs;
        let payload = serde_json::json!({"sub": user_id, "exp": exp});
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap_or_default());
        let sig = hmac_sha256(&self.secret, payload_b64.as_bytes());
        let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
        format!("{payload_b64}.{sig_b64}")
    }

    fn verify_token(&self, token: &str) -> Option<i64> {
        use base64::Engine;
        use subtle::ConstantTimeEq;

        let (payload_b64, sig_b64) = token.split_once('.')?;
        let expected = hmac_sha256(&self.secret, payload_b64.as_bytes());
        let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sig_b64)
            .ok()?;
        if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
            return None;
        }
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .ok()?;
        let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
        let exp = payload.get("exp")?.as_i64()?;
        if chrono::Utc::now().timestamp() >= exp {
            return None; // expired
        }
        payload.get("sub")?.as_i64()
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

        // JWT has exactly one dot (payload.sig). API key has one dot (prefix.secret).
        // Both are one dot — distinguish by prefix length (JWT payload is base64, not 8 hex chars).
        if token.chars().filter(|&c| c == '.').count() != 1 {
            return Ok(None);
        }
        // If the part before the first dot is exactly 8 chars, it's an API key prefix.
        if token.split_once('.').map(|(p, _)| p.len()) == Some(8) {
            return Ok(None);
        }

        let user_id = match self.verify_token(token) {
            Some(id) => id,
            None => return Err(AuthError::InvalidToken),
        };

        let users = super::auth::User::objects()
            .where_(super::auth::User::id.eq(user_id))
            .fetch_pool(pool)
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
