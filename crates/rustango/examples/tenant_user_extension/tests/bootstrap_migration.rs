//! Sanity check: the tenant-user schema for a custom `AppUser` carries
//! the extra columns, so the generated tenant-scope system migration
//! (makemigrations → `system/migrations/`) creates them.
//!
//! v0.47 — the framework no longer emits a hand-built bootstrap
//! migration; its schema flows through `SchemaSnapshot::from_models`
//! (the same path `makemigrations` uses). This test builds that
//! snapshot directly from `AppUser::SCHEMA`.
//!
//! No DB needed — pure schema math.

use rustango::core::Model as _;
use rustango::migrate::SchemaSnapshot;
use rustango::tenancy;
use tenant_user_extension::models::AppUser;

#[test]
fn tenant_bootstrap_includes_extras() {
    let snapshot = SchemaSnapshot::from_models(&[AppUser::SCHEMA]);
    let users = snapshot
        .table("rustango_users")
        .expect("rustango_users table in snapshot");

    let cols: Vec<&str> = users.fields.iter().map(|f| f.column.as_str()).collect();

    // Framework-required columns still present
    for required in tenancy::REQUIRED_USER_COLUMNS {
        assert!(
            cols.contains(required),
            "missing framework-required column {required}; got {cols:?}"
        );
    }

    // Extras present
    assert!(cols.contains(&"display_name"), "extras missing: {cols:?}");
    assert!(cols.contains(&"timezone"), "extras missing: {cols:?}");
}

#[test]
fn validate_accepts_app_user() {
    use rustango::core::Model as _;
    tenancy::validate_tenant_user_schema(&AppUser::SCHEMA)
        .expect("AppUser must satisfy TenantUserModel contract");
}
