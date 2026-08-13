//! A downstream model may own a framework table (#1168).
//!
//! The documented custom-user-model path has a project declare
//! `rustango_users` with extra columns. The built-in `User` derive is
//! unconditional, so both models land in the inventory — and the system
//! snapshot used to take *both*, producing a duplicate/ambiguous
//! `CREATE TABLE rustango_users`. Net effect: no supported way to add a
//! column to the tenant user, and the documented extension point was dead.
//!
//! Exactly one model must own a table, and when a project spells out a
//! framework table it is overriding it on purpose — so the downstream model
//! wins.

#![cfg(feature = "tenancy")]

use rustango::migrate::SchemaSnapshot;
use rustango::sql::Auto;
use rustango::Model;

/// Mirrors the `TenantUserModel` doc example: the framework's required
/// columns plus a project-specific one.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_users")]
pub struct AppUser {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 150, unique)]
    pub username: String,
    #[rustango(max_length = 255)]
    pub password_hash: String,
    pub active: bool,
    pub is_superuser: bool,
    /// The whole point: an extra typed column on the tenant user.
    #[rustango(max_length = 100)]
    pub display_name: String,
}

/// `rustango_users` appears exactly once, and it is the project's version.
#[test]
fn downstream_model_owns_the_framework_table() {
    let snap = SchemaSnapshot::from_registry_system_for_scope(rustango::core::ModelScope::Tenant);

    let users: Vec<_> = snap
        .tables
        .iter()
        .filter(|t| t.name == "rustango_users")
        .collect();
    assert_eq!(
        users.len(),
        1,
        "exactly one model must own rustango_users — two would emit a \
         duplicate CREATE TABLE"
    );

    let cols: Vec<&str> = users[0].fields.iter().map(|f| f.column.as_str()).collect();
    assert!(
        cols.contains(&"display_name"),
        "the project's model must win, so its extra column is in the \
         migration; got columns: {cols:?}"
    );
}

/// Overriding one table must not drop the framework's other tables.
#[test]
fn other_framework_tables_are_untouched() {
    let snap = SchemaSnapshot::from_registry_system_for_scope(rustango::core::ModelScope::Tenant);
    let names: Vec<&str> = snap.tables.iter().map(|t| t.name.as_str()).collect();
    for expected in ["rustango_roles", "rustango_permissions"] {
        assert!(
            names.contains(&expected),
            "{expected} must still be present; got {names:?}"
        );
    }
    // And every table is unique — the dedup applies across the board.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "no table may appear twice: {names:?}");
}
