//! User model for the showcase accounts app. Standalone — does NOT
//! piggy-back on `tenancy`'s built-in User table, since the showcase
//! runs non-tenancy.

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "showcase_accounts_user", display = "username")]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 64, unique)]
    pub username: String,

    #[rustango(max_length = 254, unique)]
    pub email: String,

    /// Argon2id phc-string. Never echoed in API responses.
    #[rustango(max_length = 200)]
    pub password_hash: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}
