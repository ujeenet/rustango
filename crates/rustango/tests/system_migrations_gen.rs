#![cfg(all(feature = "sqlite", feature = "tenancy"))]
use rustango::core::ModelScope;
#[test]
fn system_migrations_generate_framework_tables_by_scope() {
    let root = std::env::temp_dir().join(format!("rustango_sysmig_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let reg = rustango::migrate::make_migrations_system(&root, ModelScope::Registry, None)
        .unwrap()
        .expect("registry system migration");
    let ten = rustango::migrate::make_migrations_system(&root, ModelScope::Tenant, None)
        .unwrap()
        .expect("tenant system migration");

    let reg_ops: Vec<String> = reg.forward.iter().map(|o| format!("{o:?}")).collect();
    let ten_ops: Vec<String> = ten.forward.iter().map(|o| format!("{o:?}")).collect();
    eprintln!("REGISTRY forward: {reg_ops:#?}");
    eprintln!("TENANT forward: {ten_ops:#?}");

    // Registry scope owns orgs + operators; tenant scope owns users/roles/permissions.
    assert!(
        reg_ops.iter().any(|s| s.contains("rustango_orgs")),
        "registry should create orgs"
    );
    assert!(
        reg_ops.iter().any(|s| s.contains("rustango_operators")),
        "registry should create operators"
    );
    assert!(
        !reg_ops.iter().any(|s| s.contains("rustango_users")),
        "registry must NOT own users"
    );
    assert!(
        ten_ops.iter().any(|s| s.contains("rustango_users")),
        "tenant should create users"
    );
    assert!(
        ten_ops.iter().any(|s| s.contains("rustango_permissions")),
        "tenant should create permissions"
    );

    // Files land under system/migrations/, scope-tagged, and a re-run is a no-op.
    let sysdir = root.join("system").join("migrations");
    let n = std::fs::read_dir(&sysdir).unwrap().count();
    assert_eq!(n, 2, "one registry + one tenant file");
    assert!(
        rustango::migrate::make_migrations_system(&root, ModelScope::Registry, None)
            .unwrap()
            .is_none(),
        "re-run registry = no-op"
    );
    assert!(
        rustango::migrate::make_migrations_system(&root, ModelScope::Tenant, None)
            .unwrap()
            .is_none(),
        "re-run tenant = no-op"
    );

    let _ = std::fs::remove_dir_all(&root);
}
