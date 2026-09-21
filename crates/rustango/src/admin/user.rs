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
}

impl AdminUser {
    /// Hash the password and build an `AdminUser` ready to insert.
    ///
    /// # Errors
    /// [`crate::passwords::PasswordError`] from `passwords::hash`.
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
        })
    }
}
