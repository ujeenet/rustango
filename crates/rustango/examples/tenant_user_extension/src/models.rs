//! [`AppUser`] — replaces the framework's `User` for tenant-scoped
//! `rustango_users` storage. Mirrors every framework-required column
//! verbatim, then tacks on `display_name` and `timezone`.
//!
//! The framework's auth and admin paths read the seven core columns
//! by name; the extras here are for the application to use via the
//! ORM (`AppUser::objects().fetch_on(...)`).

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_users",
    display = "username",
    admin(
        list_display    = "username, display_name, timezone, is_superuser, active, created_at",
        search_fields   = "username, display_name",
        ordering        = "username",
        readonly_fields = "password_hash, created_at",
    ),
)]
pub struct AppUser {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)]
    pub username: String,
    #[rustango(max_length = 255)]
    pub password_hash: String,
    pub is_superuser: bool,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
    // ---------- extras ----------
    /// Free-form display name shown in greetings / mentions. Defaults
    /// to empty string so existing rows can apply the bootstrap
    /// without backfill.
    #[rustango(max_length = 128, default = "''")]
    pub display_name: String,
    /// IANA timezone name. Defaults to `'UTC'` so existing rows can
    /// apply the bootstrap without backfill.
    #[rustango(max_length = 64, default = "'UTC'")]
    pub timezone: String,
}

// Marker impl — opts AppUser in as a valid `rustango_users` schema.
// Validated at `init-tenancy` time via
// [`rustango::tenancy::validate_tenant_user_schema`].
impl rustango::tenancy::TenantUserModel for AppUser {}
