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
    /// Stamped on logout: sessions issued at or before it are refused, on
    /// every device (#1855).
    pub sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
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
        readonly_fields = "created_at, password_changed_at, sessions_revoked_at",
        formfield_overrides = "password_hash: password",
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
    /// Email used to link an external SSO identity (OIDC / social
    /// OAuth) to this tenant user. Both the admin/tenant-console login
    /// and the member login match the IdP's verified email against this
    /// column; `None` means the account can't sign in via SSO. Unique
    /// within the tenant, but multiple `NULL`s are allowed.
    ///
    /// `#[cfg(feature = "sso")]`-gated (the admin-independent SSO core,
    /// which `admin-sso` also enables) — enabling/disabling SSO generates
    /// an Add/DropColumn migration for this column.
    #[cfg(feature = "sso")]
    #[rustango(max_length = 254, unique)]
    pub email: Option<String>,
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
    /// Stamped on logout: sessions issued at or before it are refused, on
    /// every device (#1855).
    pub sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

// The admin form's password is hashed here; a change also stamps
// `password_changed_at`, which ends the user's older sessions.
#[cfg(feature = "admin")]
fn admin_hash_password<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    use crate::core::SqlValue;
    Box::pin(async move {
        let Some(plain) = crate::admin::derived_fields::take_secret(values, "password_hash") else {
            return Ok(());
        };
        // A model on this table without the password widget echoes the hash.
        if before.and_then(|r| r.get("password_hash")?.as_str()) == Some(plain.as_str()) {
            return Ok(());
        }
        let hash = password::hash_async(&plain)
            .await
            .map_err(|e| e.to_string())?;
        values.push(("password_hash", SqlValue::String(hash)));
        if before.is_some() {
            values.retain(|(c, _)| *c != "password_changed_at");
            values.push((
                "password_changed_at",
                SqlValue::DateTime(chrono::Utc::now()),
            ));
        }
        Ok::<(), String>(())
    })
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_users",
        derive: admin_hash_password,
    }
}

// One role must not grant superuser or take another account (#2521).
#[cfg(feature = "admin")]
crate::admin::object_permissions::lock_user_credentials!(User, "rustango_users");
#[cfg(all(feature = "admin", feature = "sso"))]
crate::register_admin_superuser_fields!(User, Add, [email]);
#[cfg(all(feature = "admin", feature = "sso"))]
crate::register_admin_superuser_fields!(User, Change, [email]);

/// A row whose password matched, before any second factor. A weak stored
/// hash is replaced only by `complete`, so a password alone cannot change
/// the fingerprint that ends the user's other sessions (#2093).
/// Fingerprint a session from the hash `complete` returns, not this one.
#[must_use = "call `complete` once every factor has passed"]
#[derive(Debug)]
pub struct PasswordVerified<T> {
    row: T,
    /// A current-cost hash of the same password, stored by `complete`.
    upgrade: Option<String>,
}

impl<T> std::ops::Deref for PasswordVerified<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.row
    }
}

impl PasswordVerified<User> {
    /// The user, its weak hash upgraded. Call after the second factor.
    #[must_use]
    pub async fn complete(self, pool: &crate::sql::Pool) -> User {
        let Self { mut row, upgrade } = self;
        if let Some(new) = upgrade {
            let id = row.id.get().copied().unwrap_or_default();
            row.password_hash = crate::passwords::store_rehash(
                pool,
                <User as crate::core::Model>::SCHEMA,
                id,
                &row.password_hash,
                new,
            )
            .await;
        }
        row
    }

    /// [`Self::complete`] on a schema-scoped connection.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub async fn complete_on(self, conn: &mut PgConnection) -> User {
        let Self { mut row, upgrade } = self;
        if let Some(new) = upgrade {
            let model = <User as crate::core::Model>::SCHEMA;
            let id = row.id.get().copied().unwrap_or_default();
            let q = crate::passwords::rehash_update(model, id, &row.password_hash, &new);
            let applied = crate::sql::update_on(&mut *conn, &q).await;
            row.password_hash =
                crate::passwords::rehash_applied(applied, model, id, &row.password_hash, new);
        }
        row
    }
}

impl PasswordVerified<Operator> {
    /// The operator, its weak hash upgraded. Call after the second factor.
    #[must_use]
    pub async fn complete(self, registry: &crate::sql::Pool) -> Operator {
        let Self { mut row, upgrade } = self;
        if let Some(new) = upgrade {
            let id = row.id.get().copied().unwrap_or_default();
            row.password_hash = crate::passwords::store_rehash(
                registry,
                <Operator as crate::core::Model>::SCHEMA,
                id,
                &row.password_hash,
                new,
            )
            .await;
        }
        row
    }
}

/// Look up an operator by username and verify the password.
///
/// Returns `Ok(Some(_))` on success — call [`PasswordVerified::complete`]
/// once any second factor passes — and `Ok(None)` for an unknown
/// username, a wrong password, OR an inactive (`active = false`)
/// operator — always the same `Ok(None)`. The unknown-username path
/// runs a dummy Argon2 verify ([`password::verify_dummy_async`]) and the
/// active check happens *after* the real verify, so response timing
/// doesn't reveal whether the account exists (audit H1).
///
/// # Errors
/// Returns [`TenancyError::Driver`]/[`TenancyError::Exec`] for SQL
/// failures, [`TenancyError::Validation`] for malformed stored
/// hashes (corrupt row), or [`TenancyError::Busy`] for a known and an
/// unknown username alike when no hashing slot frees up.
#[cfg(feature = "postgres")]
pub async fn authenticate_operator(
    registry: &PgPool,
    username: &str,
    password: &str,
) -> Result<Option<PasswordVerified<Operator>>, TenancyError> {
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
) -> Result<Option<PasswordVerified<Operator>>, TenancyError> {
    let op = find_operator(registry, username).await?;
    check_operator_password(op, password).await
}

/// The operator row for `username`, active or not.
pub(crate) async fn find_operator(
    registry: &crate::sql::Pool,
    username: &str,
) -> Result<Option<Operator>, TenancyError> {
    use crate::sql::FetcherPool as _;
    let rows: Vec<Operator> = Operator::objects()
        .where_(Operator::username.eq(username.to_owned()))
        .fetch(registry)
        .await?;
    Ok(rows.into_iter().next())
}

/// `Some(op)` when `op` is active and `password` matches; the same work
/// for a missing row.
pub(crate) async fn check_operator_password(
    op: Option<Operator>,
    password: &str,
) -> Result<Option<PasswordVerified<Operator>>, TenancyError> {
    let Some(op) = op else {
        // H1: spend a verify's worth of work on the unknown-user path
        // so timing doesn't reveal whether the account exists.
        password::verify_dummy_async(password).await?;
        return Ok(None);
    };
    // Verify before the active check so active vs inactive accounts
    // take the same time (audit H1).
    let password_ok = password::verify_async(password, &op.password_hash).await?;
    if !op.active || !password_ok {
        return Ok(None);
    }
    let upgrade = crate::passwords::rehash_async(password, &op.password_hash).await;
    Ok(Some(PasswordVerified { row: op, upgrade }))
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
) -> Result<Option<PasswordVerified<User>>, TenancyError> {
    // Through the ORM, so every column (cut-offs included) decodes or errors.
    let rows: Vec<User> = User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch_on(&mut *conn)
        .await?;
    check_user_password(rows.into_iter().next(), password).await
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
) -> Result<Option<PasswordVerified<User>>, TenancyError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    let rows: Vec<User> = User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch(pool)
        .await?;
    check_user_password(rows.into_iter().next(), password).await
}

/// [`check_operator_password`] for a tenant user.
async fn check_user_password(
    user: Option<User>,
    password: &str,
) -> Result<Option<PasswordVerified<User>>, TenancyError> {
    let Some(user) = user else {
        // H1: equalize timing for the unknown-user path.
        password::verify_dummy_async(password).await?;
        return Ok(None);
    };
    let password_ok = password::verify_async(password, &user.password_hash).await?;
    if !user.active || !password_ok {
        return Ok(None);
    }
    let upgrade = crate::passwords::rehash_async(password, &user.password_hash).await;
    Ok(Some(PasswordVerified { row: user, upgrade }))
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
/// Declaring the model on `rustango_users` is what makes it the user
/// table: `makemigrations` builds the table from it. Register it instead
/// of the framework [`User`], never beside it.
/// [`crate::manage::Cli::user_model`] checks this contract at startup (#1203).
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
///     pub password_changed_at: Option<chrono::DateTime<chrono::Utc>>,
///     pub sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
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
    // Read by every session check (#1338, #2036).
    "password_changed_at",
    "sessions_revoked_at",
];

/// Validate that `schema` is a viable `rustango_users` model — same
/// table name and all [`REQUIRED_USER_COLUMNS`] present. `user_model`
/// calls it, so a bad override fails at startup, not at first login.
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

    impl TenantUserModel for MissingDataColumn {}

    /// #1203 — `user_model` is a startup check, not a no-op.
    #[cfg(feature = "manage")]
    #[test]
    #[should_panic(expected = "missing required column")]
    fn user_model_refuses_a_model_missing_a_column() {
        let _ = crate::manage::Cli::new().user_model::<MissingDataColumn>();
    }

    #[test]
    fn validate_rejects_missing_required_column() {
        use crate::core::Model as _;
        let err = validate_tenant_user_schema(&MissingDataColumn::SCHEMA).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("data"), "{msg}");
    }
}
