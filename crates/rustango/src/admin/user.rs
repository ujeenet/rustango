//! `AdminUser`: the credential store for the bare admin's session
//! auth.
//!
//! One small table: username, password hash, superuser flag, active
//! flag and a created-at timestamp. Just enough for `POST /login` via
//! [`crate::passwords::verify`] and the sidebar's visibility check.
//!
//! An app that needs roles, permissions or profile data adds its own
//! tables, or moves to the `tenancy` feature and its `tenancy::User`.

use crate::sql::Auto;
use crate::Model;

/// Admin operator account. [`crate::admin::Builder::with_session_auth`]
/// creates the table at bootstrap if it is missing.
///
/// `username` is the login key and a UNIQUE index keeps it unique.
/// `password_hash` is the argon2id PHC string from
/// [`crate::passwords::hash`].
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_admin_users",
    app = "admin",
    display = "username",
    admin(
        list_display = "username, is_superuser, active, created_at",
        search_fields = "username",
        ordering = "username",
        readonly_fields = "created_at, sessions_revoked_at",
        formfield_overrides = "password_hash: password",
    )
)]
pub struct AdminUser {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 150, unique)]
    pub username: String,
    /// argon2id PHC string from [`crate::passwords::hash`].
    #[rustango(max_length = 200)]
    pub password_hash: String,
    /// Email that links an external SSO identity to this account. The
    /// `admin-sso` login matches the IdP's **verified** email against
    /// this column. `None` means the account cannot use SSO. Unique,
    /// though several rows may be `NULL`.
    ///
    /// The column exists only with the `admin-sso` feature, so turning
    /// the feature on or off produces an Add/DropColumn migration.
    #[cfg(feature = "admin-sso")]
    #[rustango(max_length = 254, unique)]
    pub email: Option<String>,
    /// `true` grants full admin access. The bare admin's gate is set
    /// to superuser-only by default, so a `false` session gets a 403.
    #[rustango(default = "false")]
    pub is_superuser: bool,
    /// Soft-disable flag. Set it to `false` to lock the account out
    /// without deleting it. The login handler rejects inactive users.
    #[rustango(default = "true")]
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Stamped on logout: sessions issued at or before it are refused, on
    /// every device (#1855).
    pub sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

// The admin form's password is hashed here, off the runtime.
fn admin_hash_password<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> super::derived_fields::DeriveFuture<'a> {
    Box::pin(async move {
        let Some(plain) = super::derived_fields::take_secret(values, "password_hash") else {
            return Ok(());
        };
        // A model on this table without the password widget echoes the hash.
        if before.and_then(|r| r.get("password_hash")?.as_str()) == Some(plain.as_str()) {
            return Ok(());
        }
        let hash = crate::passwords::hash_async(&plain)
            .await
            .map_err(|e| e.to_string())?;
        values.push(("password_hash", crate::core::SqlValue::String(hash)));
        Ok::<(), String>(())
    })
}

inventory::submit! {
    super::derived_fields::AdminDerivedField {
        table: "rustango_admin_users",
        derive: admin_hash_password,
    }
}

super::object_permissions::lock_user_credentials!(AdminUser, "rustango_admin_users");
#[cfg(feature = "admin-sso")]
crate::register_admin_superuser_fields!(AdminUser, Add, [email]);
#[cfg(feature = "admin-sso")]
crate::register_admin_superuser_fields!(AdminUser, Change, [email]);

impl AdminUser {
    /// Hash the password and build an `AdminUser` ready to insert.
    /// Sync: in a request handler, hash with `passwords::hash_async` instead.
    ///
    /// # Errors
    /// [`crate::passwords::PasswordError`] from `passwords::hash`.
    #[allow(clippy::disallowed_methods)]
    pub fn new_with_password(
        username: impl Into<String>,
        password: &str,
        is_superuser: bool,
    ) -> Result<Self, crate::passwords::PasswordError> {
        Ok(Self {
            id: Auto::Unset,
            username: username.into(),
            password_hash: crate::passwords::hash(password)?,
            #[cfg(feature = "admin-sso")]
            email: None,
            is_superuser,
            active: true,
            created_at: chrono::Utc::now(),
            sessions_revoked_at: None,
        })
    }
}
