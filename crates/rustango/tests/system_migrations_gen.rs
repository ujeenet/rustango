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

#[tokio::test]
async fn system_migrations_apply_cleanly_on_sqlite() {
    use rustango::sql::{sqlx, Pool};
    let root = std::env::temp_dir().join(format!("rustango_sysapply_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    rustango::migrate::make_migrations_system(&root, ModelScope::Registry, None).unwrap();
    rustango::migrate::make_migrations_system(&root, ModelScope::Tenant, None).unwrap();
    let sysdir = root.join("system").join("migrations");

    let dbpath = root.join("t.db");
    let url = format!("sqlite:{}?mode=rwc", dbpath.display());
    let pool = Pool::connect(&url).await.unwrap();
    let applied = rustango::migrate::migrate_pool(&pool, &sysdir)
        .await
        .unwrap();
    assert_eq!(applied.len(), 2, "two system migrations applied");

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    for t in [
        "rustango_orgs",
        "rustango_operators",
        "rustango_users",
        "rustango_roles",
        "rustango_permissions",
        "rustango_role_permissions",
        "rustango_user_roles",
        "rustango_user_permissions",
        "rustango_api_keys",
        "rustango_audit_log",
        "rustango_content_types",
        "rustango_admin_users",
    ] {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(t)
                .fetch_one(sq)
                .await
                .unwrap();
        assert_eq!(
            n, 1,
            "table {t} must exist after applying system migrations"
        );
    }
    let idx: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='rustango_permissions_table_name_codename_idx'",
    ).fetch_one(sq).await.unwrap();
    assert_eq!(idx, 1, "composite unique index must exist");

    // Re-run is idempotent (ledger tracks applied).
    assert!(rustango::migrate::migrate_pool(&pool, &sysdir)
        .await
        .unwrap()
        .is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(feature = "admin-sso")]
#[test]
fn admin_sso_feature_toggles_add_and_drop_columns() {
    use rustango::migrate::{detect_changes, SchemaSnapshot};
    // Feature ON → the framework Org snapshot carries the sso_* columns.
    let with_sso = SchemaSnapshot::from_registry_system_for_scope(ModelScope::Registry);
    // Simulate feature OFF by stripping the sso_* columns.
    let mut without_sso = with_sso.clone();
    for t in without_sso
        .tables
        .iter_mut()
        .filter(|t| t.name == "rustango_orgs")
    {
        t.fields.retain(|f| !f.column.starts_with("sso_"));
    }
    assert_ne!(
        with_sso, without_sso,
        "sso_* columns must exist when admin-sso is on"
    );

    // Enabling the feature (off → on) generates AddColumn migrations;
    // disabling (on → off) generates DropColumn migrations.
    let enable = format!("{:?}", detect_changes(&without_sso, &with_sso));
    let disable = format!("{:?}", detect_changes(&with_sso, &without_sso));
    eprintln!("ENABLE ops: {enable}");
    eprintln!("DISABLE ops: {disable}");
    assert!(
        enable.contains("AddColumn") && enable.contains("sso_enabled"),
        "enabling admin-sso must AddColumn sso_*: {enable}"
    );
    assert!(
        disable.contains("DropColumn") && disable.contains("sso_enabled"),
        "disabling admin-sso must DropColumn sso_*: {disable}"
    );
}
