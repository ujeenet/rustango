//! Sanity check: the bootstrap migration emitted via
//! `init_tenancy_with::<AppUser>` carries the extra columns.
//!
//! No DB needed — pure schema math.

use rustango::tenancy;
use tenant_user_extension::models::AppUser;

#[test]
fn tenant_bootstrap_includes_extras() {
    let mig = tenancy::tenant_bootstrap_migration_for::<AppUser>();
    let users = mig
        .snapshot
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
