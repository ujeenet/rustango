//! `AdminUser` — credential store for the bare admin's session-auth
//! layer. Issue #253 slice A.
//!
//! Single-table user store, deliberately tiny — username, password
//! hash, superuser flag, active flag, created-at timestamp. Holds the
//! bare minimum to support `POST /login` against
//! [`crate::passwords::verify`] and a sidebar visibility check.
//!
//! Apps that need richer user shapes (roles, permissions, profile
//! data) layer those on top in their own tables, or graduate to the
//! `tenancy` feature where the full `tenancy::User` lives.

use crate::sql::Auto;
use crate::Model;

/// Admin operator account. The table is created on bootstrap by
/// [`crate::admin::Builder::with_session_auth`] when the schema
/// isn't already present (mirrors the bootstrap helper
/// `crate::server::AppBuilder::bootstrap` shape).
///
/// `username` is the natural login key; the table-level UNIQUE
/// index enforces no duplicates. `password_hash` is whatever
/// [`crate::passwords::hash`] produces (argon2id PHC string).
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
    /// Email used to link an external SSO identity (OIDC / social
    /// OAuth) to this account. The `admin-sso` login matches the IdP's
    /// verified email against this column; `None` means the account
    /// can't sign in via SSO. Unique, but multiple `NULL`s are allowed.
    #[rustango(max_length = 254, unique)]
    pub email: Option<String>,
    /// `true` = full admin access. `false` = future-slice's
    /// filtered-model view; today honored only by the sidebar.
    #[rustango(default = "false")]
    pub is_superuser: bool,
    /// Soft-disable flag — set to `false` to lock out without
    /// deleting. The login handler rejects inactive users.
    #[rustango(default = "true")]
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl AdminUser {
    /// Hash the password and build a fresh `AdminUser` ready for
    /// `.insert(pool)`. Returns the propagated `crate::passwords::Error`
    /// when hashing fails.
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
            email: None,
            is_superuser,
            active: true,
            created_at: chrono::Utc::now(),
        })
    }
}
