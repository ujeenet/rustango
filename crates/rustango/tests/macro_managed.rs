//! Django-parity #321 — `#[rustango(managed = false)]` attribute on
//! `#[derive(Model)]`. Verifies the attribute parses and threads
//! through to `ModelSchema::managed`.

#![cfg(feature = "sqlite")]

use rustango::migrate::SchemaSnapshot;
use rustango::Model;

#[derive(Model)]
#[rustango(table = "mng_managed_post")]
#[allow(dead_code)]
pub struct ManagedPost {
    #[rustango(primary_key)]
    id: i64,
    title: String,
}

#[derive(Model)]
#[rustango(table = "mng_unmanaged_legacy", managed = false)]
#[allow(dead_code)]
pub struct UnmanagedLegacy {
    #[rustango(primary_key)]
    id: i64,
    legacy_label: String,
}

#[test]
fn managed_default_is_true() {
    assert!(
        <ManagedPost as rustango::core::Model>::SCHEMA.managed,
        "default `managed` must be true"
    );
}

#[test]
fn explicit_managed_false_is_threaded_to_schema() {
    assert!(
        !<UnmanagedLegacy as rustango::core::Model>::SCHEMA.managed,
        "`#[rustango(managed = false)]` must set ModelSchema.managed to false"
    );
}

#[test]
fn unmanaged_models_skipped_from_migration_snapshot() {
    let snap = SchemaSnapshot::from_models(&[
        <ManagedPost as rustango::core::Model>::SCHEMA,
        <UnmanagedLegacy as rustango::core::Model>::SCHEMA,
    ]);
    let names: Vec<&str> = snap.tables.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.iter().any(|n| *n == "mng_managed_post"),
        "managed table must appear: {names:?}"
    );
    assert!(
        !names.iter().any(|n| *n == "mng_unmanaged_legacy"),
        "unmanaged table must NOT appear: {names:?}"
    );
}
