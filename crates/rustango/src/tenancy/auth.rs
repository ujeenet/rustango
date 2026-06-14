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
//! Use `cargo run -- create-operator <username> --password <p>` and
//! `cargo run -- create-user <slug> <username> --password <p>
//! [--superuser]` — see [`super::manage`]. (Replace `cargo run` with
//! your project's binary name if different; the verbs route through
//! `rustango::manage::Cli`.)

use crate::core::Column as _;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::{PgConnection, PgPool};
#[allow(unused_imports)]
use crate::sql::Auto;
use crate::Model;
use base64::Engine;

use super::error::TenancyError;
use super::password;

/// Registry-scoped operator. Single identity domain for the main app
/// administrator(s); not visible to tenants.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_operators", display = "username", scope = "registry")]
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
    /// Timestamp of the last password rotation (v0.28.4, #77).
    /// Set on every reset / change-password verb. Sessions whose
    /// `iat` (issued-at) is strictly less than this value are
    /// rejected by `validate_session`. `None` for accounts that
    /// haven't rotated since v0.28.4 — those sessions stay valid.
    pub password_changed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Per-tenant user. Lives in the tenant's storage (schema or
/// dedicated DB). `is_superuser = true` elevates to org-admin inside
/// the tenant — never grants access to `/operator`.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_users",
    display = "username",
    admin(
        list_display = "username, is_superuser, active, created_at",
        search_fields = "username",
        ordering = "username",
        readonly_fields = "password_hash, created_at",
    )
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
    /// Timestamp of the last password rotation (v0.28.4, #77).
    /// Set on every reset / change-password verb. Sessions whose
    /// `iat` is strictly less than this value are rejected by
    /// `validate_session`. `None` for accounts that haven't
    /// rotated since v0.28.4 — those sessions stay valid until
    /// they expire normally.
    pub password_changed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Look up an operator by username and verify the password.
///
/// Returns `Ok(Some(operator))` on success, `Ok(None)` for an unknown
/// username, a wrong password, OR an inactive (`active = false`)
/// operator — always the same `Ok(None)`. The unknown-username path
/// runs a dummy Argon2 verify ([`password::verify_dummy`]) and the
/// active check happens *after* the real verify, so response timing
/// doesn't reveal whether the account exists (audit H1).
///
/// # Errors
/// Returns [`TenancyError::Driver`]/[`TenancyError::Exec`] for SQL
/// failures, or [`TenancyError::Validation`] for malformed stored
/// hashes (corrupt row).
#[cfg(feature = "postgres")]
pub async fn authenticate_operator(
    registry: &PgPool,
    username: &str,
    password: &str,
) -> Result<Option<Operator>, TenancyError> {
    authenticate_operator_pool(
        &crate::sql::Pool::Postgres(registry.clone()),
        username,
        password,
    )
    .await
}

/// Backend-agnostic counterpart of [`authenticate_operator`]. Routes
/// the Operator lookup through [`crate::sql::FetcherPool`] so the same
/// auth path works against `Pool::Postgres` / `Pool::Mysql` /
/// `Pool::Sqlite`. Preferred for new code; the PG-typed wrapper above
/// keeps existing callers compiling.
///
/// # Errors
/// As [`authenticate_operator`].
pub async fn authenticate_operator_pool(
    registry: &crate::sql::Pool,
    username: &str,
    password: &str,
) -> Result<Option<Operator>, TenancyError> {
    use crate::sql::FetcherPool as _;
    let rows: Vec<Operator> = Operator::objects()
        .where_(Operator::username.eq(username.to_owned()))
        .fetch(registry)
        .await?;
    let Some(op) = rows.into_iter().next() else {
        // H1: spend a verify's worth of work on the unknown-user path
        // so timing doesn't reveal whether the account exists.
        password::verify_dummy(password);
        return Ok(None);
    };
    // Verify before the active check so active vs inactive accounts
    // take the same time (audit H1).
    let password_ok = password::verify(password, &op.password_hash)?;
    if !op.active || !password_ok {
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
/// v0.38 — kept as the PG-only entry point that runs against a raw
/// `&mut PgConnection` for schema-mode tenants whose `search_path` is
/// set on the connection. New code on any backend should reach for
/// [`authenticate_user_pool`] instead, which takes the unified
/// [`crate::sql::Pool`] enum and works on PG / SQLite / MySQL.
///
/// # Errors
/// As [`authenticate_operator`].
#[cfg(feature = "postgres")]
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
        // H1: equalize timing for the unknown-user path.
        password::verify_dummy(password);
        return Ok(None);
    };
    let user = User {
        id: Auto::Set(row.try_get::<i64, _>("id")?),
        username: row.try_get::<String, _>("username")?,
        password_hash: row.try_get::<String, _>("password_hash")?,
        is_superuser: row.try_get::<bool, _>("is_superuser")?,
        active: row.try_get::<bool, _>("active")?,
        created_at: row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        data: row
            .try_get::<serde_json::Value, _>("data")
            .unwrap_or_else(|_| serde_json::json!({})),
        password_changed_at: row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("password_changed_at")
            .ok()
            .flatten(),
    };
    let password_ok = password::verify(password, &user.password_hash)?;
    if !user.active || !password_ok {
        return Ok(None);
    }
    Ok(Some(user))
}

/// Tri-dialect counterpart of [`authenticate_user`] (v0.38). Takes the
/// unified [`crate::sql::Pool`] enum so the same body runs on PG,
/// SQLite, and MySQL — the underlying ORM `_pool` helper picks the
/// right placeholder + identifier-quoting rules per dialect.
///
/// `pool` must point at the tenant's storage:
/// * Database-mode (any backend): a cached `sqlx::Pool<DB>` for the
///   tenant DB.
/// * Schema-mode (PG-only by language): a short-lived `PgPool` whose
///   `after_connect` set `search_path` to the tenant's schema —
///   typically built via
///   [`super::TenantPools::scoped_pool_dyn`].
///
/// Same return semantics as [`authenticate_operator`]: `Ok(None)`
/// for unknown user, wrong password, or inactive row; `Ok(Some(_))`
/// only on a successful match.
///
/// # Errors
/// As [`authenticate_operator_pool`].
pub async fn authenticate_user_pool(
    pool: &crate::sql::Pool,
    username: &str,
    password: &str,
) -> Result<Option<User>, TenancyError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    let rows: Vec<User> = User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch(pool)
        .await?;
    let Some(user) = rows.into_iter().next() else {
        // H1: equalize timing for the unknown-user path.
        password::verify_dummy(password);
        return Ok(None);
    };
    let password_ok = password::verify(password, &user.password_hash)?;
    if !user.active || !password_ok {
        return Ok(None);
    }
    Ok(Some(user))
}

// ---------- Swappable user model ----------

/// Marker trait for the model backing `rustango_users` in a tenant's
/// storage. The framework's [`User`] implements it as the default.
///
/// Implement it on your own `#[derive(Model)]` struct when you want the
/// tenant `rustango_users` table to carry **extra columns** beyond the
/// framework's defaults (display name, timezone, avatar URL, …).
///
/// ## Contract
///
/// The implementing struct's [`crate::core::ModelSchema`] MUST:
/// * have `table = "rustango_users"`,
/// * include every column in [`REQUIRED_USER_COLUMNS`] with a compatible
///   Rust type (the framework's auth path reads these by name).
///
/// Extras must be NULL-able or carry a `default = "…"` so existing
/// tenants can run the bootstrap migration without per-row backfill.
///
/// Wire your model in via [`crate::manage::Cli::user_model`].
///
/// ```ignore
/// #[derive(rustango::Model)]
/// #[rustango(table = "rustango_users")]
/// pub struct AppUser {
///     #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>,
///     #[rustango(max_length = 64, unique)] pub username: String,
///     #[rustango(max_length = 255)] pub password_hash: String,
///     pub is_superuser: bool,
///     pub active: bool,
///     pub created_at: chrono::DateTime<chrono::Utc>,
///     #[rustango(default = "'{}'")] pub data: serde_json::Value,
///     // extras —
///     #[rustango(max_length = 128, default = "''")] pub display_name: String,
///     #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
/// }
/// impl rustango::tenancy::TenantUserModel for AppUser {}
/// ```
pub trait TenantUserModel: crate::core::Model {}

impl TenantUserModel for User {}

/// Column names the framework's auth/admin paths read directly from
/// `rustango_users`. A [`TenantUserModel`] schema must contain every
/// one of these.
pub const REQUIRED_USER_COLUMNS: &[&str] = &[
    "id",
    "username",
    "password_hash",
    "is_superuser",
    "active",
    "created_at",
    "data",
];

/// Validate that `schema` is a viable `rustango_users` model — same
/// table name and all [`REQUIRED_USER_COLUMNS`] present. Called by
/// the bootstrap-migration generators so a misconfigured override
/// fails fast with a clear message at `init-tenancy` time, not later
/// during a tenant create or login.
///
/// # Errors
/// Returns [`TenancyError::Validation`] when the table name is wrong
/// or a required column is missing.
pub fn validate_tenant_user_schema(schema: &crate::core::ModelSchema) -> Result<(), TenancyError> {
    if schema.table != "rustango_users" {
        return Err(TenancyError::Validation(format!(
            "TenantUserModel must point at table \"rustango_users\", got \"{}\"",
            schema.table
        )));
    }
    for required in REQUIRED_USER_COLUMNS {
        if !schema.fields.iter().any(|f| f.column == *required) {
            return Err(TenancyError::Validation(format!(
                "TenantUserModel \"{}\" is missing required column \"{}\"",
                schema.name, required
            )));
        }
    }
    Ok(())
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

    #[test]
    fn validate_accepts_default_user() {
        use crate::core::Model as _;
        validate_tenant_user_schema(&User::SCHEMA).unwrap();
    }

    #[test]
    fn validate_rejects_wrong_table() {
        use crate::core::Model as _;
        // Use Operator's schema — same shape-ish but wrong table name.
        let err = validate_tenant_user_schema(&Operator::SCHEMA).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("rustango_users"), "{msg}");
    }

    #[derive(crate::Model, Debug, Clone)]
    #[rustango(table = "rustango_users")]
    #[allow(dead_code)]
    pub struct MissingDataColumn {
        #[rustango(primary_key)]
        pub id: rustango::sql::Auto<i64>,
        #[rustango(max_length = 64, unique)]
        pub username: String,
        #[rustango(max_length = 255)]
        pub password_hash: String,
        pub is_superuser: bool,
        pub active: bool,
        pub created_at: chrono::DateTime<chrono::Utc>,
        // `data` deliberately omitted
    }

    #[test]
    fn validate_rejects_missing_required_column() {
        use crate::core::Model as _;
        let err = validate_tenant_user_schema(&MissingDataColumn::SCHEMA).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("data"), "{msg}");
    }
}
