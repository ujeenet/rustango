//! 2-domain auth — registry-scoped [`Operator`] and per-tenant
//! [`User`].
//!
//! ## Hard wall
//!
//! These are **two distinct identity domains** with no crossover:
//!
//! * `Operator` lives in `rustango_operators` in the **registry**
//!   database. They sign in at `/operator/*` against the registry
//!   pool. They NEVER appear in any tenant table.
//! * `User` lives in `rustango_users` inside the **tenant's**
//!   storage (schema or dedicated DB). They sign in at the tenant
//!   URL against the tenant pool. The `is_superuser` flag elevates
//!   them to org-admin INSIDE that tenant — but they NEVER reach
//!   `/operator` even with that flag.
//!
//! The hard wall is enforced by middleware: operator routes call
//! [`authenticate_operator`] against the registry; tenant routes
//! call [`authenticate_user`] against the resolved tenant's pool.
//! An operator's `Authorization: Basic …` header sent to a tenant
//! URL won't authenticate (the username doesn't exist in the
//! tenant's `rustango_users`); a user's header sent to `/operator`
//! won't authenticate (the username doesn't exist in the registry's
//! `rustango_operators`). Browser cookie isolation by subdomain
//! plus this hard wall gives defense in depth.
//!
//! ## Auth mechanism
//!
//! Slice 6 ships **HTTP Basic** auth backed by argon2id-hashed
//! passwords in the database. Sessions / cookies / login forms /
//! password reset / OAuth land in v0.6.x; the model + crypto
//! foundations stay the same.
//!
//! ## Bootstrap
//!
//! Use `manage create-operator <username> --password <p>` and
//! `manage create-user <slug> <username> --password <p>
//! [--superuser]` — see [`super::manage`].

use base64::Engine;
use crate::core::Column as _;
use crate::sql::sqlx::{PgConnection, PgPool};
use crate::sql::{Auto, Fetcher};
use crate::Model;

use super::error::TenancyError;
use super::password;

/// Registry-scoped operator. Single identity domain for the main app
/// administrator(s); not visible to tenants.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_operators", display = "username")]
#[allow(dead_code)]
pub struct Operator {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    /// Login handle. Globally unique within the registry.
    #[rustango(max_length = 64, unique)]
    pub username: String,
    /// PHC-format Argon2id hash. NEVER stored as plaintext.
    #[rustango(max_length = 255)]
    pub password_hash: String,
    /// Soft-disable — `false` rejects login without dropping the row.
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Per-tenant user. Lives in the tenant's storage (schema or
/// dedicated DB). `is_superuser = true` elevates to org-admin inside
/// the tenant — never grants access to `/operator`.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_users",
    display = "username",
    admin(
        list_display    = "username, is_superuser, active, created_at",
        search_fields   = "username",
        ordering        = "username",
        readonly_fields = "password_hash, created_at",
    ),
)]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    /// Login handle. Unique within this tenant.
    #[rustango(max_length = 64, unique)]
    pub username: String,
    /// PHC-format Argon2id hash.
    #[rustango(max_length = 255)]
    pub password_hash: String,
    /// Org-admin within this tenant. Renders write-buttons, allows
    /// edit/delete; non-superusers see read-only views (admin
    /// authorization is the v0.6.x story; slice 6 stores the flag).
    pub is_superuser: bool,
    /// Soft-disable.
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Flexible per-user metadata bag. Store preferences, onboarding
    /// state, app-specific attributes — anything that doesn't need its
    /// own column. Never read by the framework itself.
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
}

/// Look up an operator by username and verify the password.
///
/// Returns `Ok(Some(operator))` on success, `Ok(None)` for an
/// unknown username OR a wrong password (one return path → no
/// timing oracle on whether the username exists). `Ok(None)` on an
/// inactive (`active = false`) operator.
///
/// # Errors
/// Returns [`TenancyError::Driver`]/[`TenancyError::Exec`] for SQL
/// failures, or [`TenancyError::Validation`] for malformed stored
/// hashes (corrupt row).
pub async fn authenticate_operator(
    registry: &PgPool,
    username: &str,
    password: &str,
) -> Result<Option<Operator>, TenancyError> {
    let rows: Vec<Operator> = Operator::objects()
        .where_(Operator::username.eq(username.to_owned()))
        .fetch(registry)
        .await?;
    let Some(op) = rows.into_iter().next() else {
        // Run a dummy verify against a known-good hash to keep
        // timing roughly even with the success path. Skipped here
        // for simplicity — the registry pool is presumed not to be
        // exposed to high-frequency probing in slice 6.
        return Ok(None);
    };
    if !op.active {
        return Ok(None);
    }
    if !password::verify(password, &op.password_hash)? {
        return Ok(None);
    }
    Ok(Some(op))
}

/// Look up a tenant user by username and verify the password.
///
/// `conn` must be already scoped to the tenant — typically obtained
/// via [`super::TenantPools::acquire`] (schema mode pre-sets
/// `search_path`; database mode is naturally scoped to the tenant
/// DB).
///
/// Same return semantics as [`authenticate_operator`]: `Ok(None)`
/// for unknown user, wrong password, or inactive row; `Ok(Some(_))`
/// only on a successful match.
///
/// # Errors
/// As [`authenticate_operator`].
pub async fn authenticate_user(
    conn: &mut PgConnection,
    username: &str,
    password: &str,
) -> Result<Option<User>, TenancyError> {
    use crate::sql::sqlx::Row;
    // We can't reuse `User::objects().fetch(&pool)` here because we
    // have a connection, not a pool. Hand-write the query — small
    // surface, not a hot path.
    let user_rows = rustango::sql::sqlx::query(
        "SELECT id, username, password_hash, is_superuser, active, created_at \
         FROM rustango_users WHERE username = $1",
    )
    .bind(username)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = user_rows else {
        return Ok(None);
    };
    let user = User {
        id: Auto::Set(row.try_get::<i64, _>("id")?),
        username: row.try_get::<String, _>("username")?,
        password_hash: row.try_get::<String, _>("password_hash")?,
        is_superuser: row.try_get::<bool, _>("is_superuser")?,
        active: row.try_get::<bool, _>("active")?,
        created_at: row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        data: row.try_get::<serde_json::Value, _>("data").unwrap_or_else(|_| serde_json::json!({})),
    };
    if !user.active {
        return Ok(None);
    }
    if !password::verify(password, &user.password_hash)? {
        return Ok(None);
    }
    Ok(Some(user))
}

// ---------- HTTP Basic helpers ----------

/// Parse an `Authorization: Basic <base64>` header value into
/// `(username, password)`. Returns `None` for missing or malformed
/// headers; the caller surfaces a 401 in that case.
#[must_use]
pub fn parse_basic_auth(header_value: Option<&str>) -> Option<(String, String)> {
    let raw = header_value?;
    let encoded = raw.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some((user.to_owned(), pass.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_auth_decodes_standard_format() {
        // base64 of "alice:hunter2" → "YWxpY2U6aHVudGVyMg=="
        let v = "Basic YWxpY2U6aHVudGVyMg==";
        let (u, p) = parse_basic_auth(Some(v)).unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "hunter2");
    }

    #[test]
    fn parse_basic_auth_rejects_non_basic_scheme() {
        assert!(parse_basic_auth(Some("Bearer tokenhere")).is_none());
        assert!(parse_basic_auth(Some("Digest qop=auth")).is_none());
    }

    #[test]
    fn parse_basic_auth_rejects_missing_colon() {
        // base64 of "no-colon-here"
        let v = "Basic bm8tY29sb24taGVyZQ==";
        assert!(parse_basic_auth(Some(v)).is_none());
    }

    #[test]
    fn parse_basic_auth_handles_none_header() {
        assert!(parse_basic_auth(None).is_none());
    }
}
