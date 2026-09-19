//! `makemigrations` must not write a system migration for a scope
//! that owns no table of its own.
//!
//! Every scope's snapshot carries the shared tables
//! (`rustango_audit_log`, `rustango_content_types`), so "snapshot is
//! non-empty" does not mean "this scope is in use". On a build
//! without `tenancy` the registry scope holds exactly those two and
//! nothing else — and `migrate_system` only ever *applies*
//! tenant-scoped migrations, so the registry-scoped `0001` it used to
//! emit was written, never applied, and then collided with `relation
//! already exists` the day the project enabled tenancy (#1307).
//!
//! Both feature configurations are asserted in one file: the
//! behaviour flips on `tenancy`, and a test that only ran in one of
//! them would be asserting a constant.

use rustango::core::ModelScope;
use rustango::migrate::snapshot::scope_owns_system_tables;

#[test]
fn the_tenant_scope_always_owns_tables() {
    assert!(
        scope_owns_system_tables(ModelScope::Tenant),
        "the tenant scope ships admin users and media in every build; \
         if this is false the predicate is broken, not the build",
    );
}

#[cfg(not(feature = "tenancy"))]
#[test]
fn without_tenancy_the_registry_scope_owns_nothing() {
    assert!(
        !scope_owns_system_tables(ModelScope::Registry),
        "without `tenancy` the registry scope holds only the shared \
         tables, so no registry migration should be generated",
    );
}

#[cfg(feature = "tenancy")]
#[test]
fn with_tenancy_the_registry_scope_owns_tables() {
    assert!(
        scope_owns_system_tables(ModelScope::Registry),
        "`tenancy` brings rustango_orgs and friends, so the registry \
         scope must still generate its migration",
    );
}

/// The end-to-end assertion: `make_migrations_system` must write no
/// registry file. Everything above tests the predicate; this tests
/// the caller, which is where #1307 actually lived.
#[cfg(not(feature = "tenancy"))]
#[test]
fn no_registry_migration_is_written_without_tenancy() {
    let tmp = std::env::temp_dir().join(format!("rustango_1307_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("tempdir");

    let made = rustango::migrate::make_migrations_system(&tmp, ModelScope::Registry, None)
        .expect("registry pass must not error");
    assert!(
        made.is_none(),
        "no registry migration should be generated in a build with no \
         registry database; got {:?}",
        made.map(|m| m.name),
    );

    let dir = tmp.join("system").join("migrations");
    let written: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        written.is_empty(),
        "nothing should reach disk, found {written:?} in {}",
        dir.display(),
    );

    // The tenant pass is the one that must still produce a file —
    // otherwise "wrote nothing" is passing for the wrong reason.
    let tenant = rustango::migrate::make_migrations_system(&tmp, ModelScope::Tenant, None)
        .expect("tenant pass must not error");
    assert!(
        tenant.is_some(),
        "the tenant scope is the one that gets applied and must still emit",
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The reason the predicate cannot read the snapshot: the snapshot is
/// non-empty for a scope that has no database at all.
#[test]
fn a_non_empty_snapshot_does_not_mean_the_scope_is_in_use() {
    let snap =
        rustango::migrate::SchemaSnapshot::from_registry_system_for_scope(ModelScope::Registry);
    let names: Vec<&str> = snap.tables.iter().map(|t| t.name.as_str()).collect();

    for shared in rustango::migrate::snapshot::SHARED_SYSTEM_TABLES {
        assert!(
            names.contains(shared),
            "{shared} should appear in every scope's snapshot; got {names:?}",
        );
    }

    #[cfg(not(feature = "tenancy"))]
    {
        assert_eq!(
            names.len(),
            rustango::migrate::snapshot::SHARED_SYSTEM_TABLES.len(),
            "without tenancy the registry snapshot should be shared-only: {names:?}",
        );
        // The pair that matters: non-empty snapshot, unowned scope.
        // Anything deciding from the snapshot alone reintroduces #1307.
        assert!(!names.is_empty());
        assert!(!scope_owns_system_tables(ModelScope::Registry));
    }
}
