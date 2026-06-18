//! App-level authentication models.
//!
//! `User` is the kind of account table an application owns — distinct from the
//! framework's admin-operator store (`rustango_admin_users`). The password is
//! stored as an argon2id PHC hash (never plaintext); see
//! [`docs/auth-passwords.md`](../../../../docs/auth-passwords.md).

use rustango::{Auto, Model};

#[derive(Model, Clone, Debug)]
#[rustango(table = "auth_users", display = "username")]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 150, unique)]
    pub username: String,
    #[rustango(max_length = 254)]
    pub email: String,
    /// argon2id PHC string from `rustango::passwords::hash` — never plaintext.
    #[rustango(max_length = 255)]
    pub password_hash: String,
    pub is_active: bool,
    pub is_superuser: bool,
}
