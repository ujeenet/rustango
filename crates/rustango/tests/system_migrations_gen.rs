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

    // Registry-scope and tenant-scope migrations target DIFFERENT
    // storage in reality: registry-scope → the registry DB; tenant-scope
    // → each tenant's own DB (or PG schema). Per-DB shared tables
    // (`rustango_audit_log`, `rustango_content_types`) therefore appear
    // in BOTH scopes — one copy per database, never two in the same one.
    // We mirror that here with a separate root + DB per scope so the
    // shared tables don't collide (which they would if both scope files
    // were applied to a single database).
    let reg_root = root.join("reg");
    let ten_root = root.join("ten");
    std::fs::create_dir_all(&reg_root).unwrap();
    std::fs::create_dir_all(&ten_root).unwrap();
    rustango::migrate::make_migrations_system(&reg_root, ModelScope::Registry, None).unwrap();
    rustango::migrate::make_migrations_system(&ten_root, ModelScope::Tenant, None).unwrap();
    let reg_dir = reg_root.join("system").join("migrations");
    let ten_dir = ten_root.join("system").join("migrations");

    async fn apply_and_list(root: &std::path::Path, dir: &std::path::Path) -> (Pool, Vec<String>) {
        let url = format!("sqlite:{}?mode=rwc", root.join("db.sqlite").display());
        let pool = Pool::connect(&url).await.unwrap();
        let applied = rustango::migrate::migrate_pool(&pool, dir).await.unwrap();
        assert_eq!(applied.len(), 1, "one system migration applied for {dir:?}");
        let Pool::Sqlite(sq) = &pool else {
            unreachable!()
        };
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'rustango\\_%' ESCAPE '\\'",
        )
        .fetch_all(sq)
        .await
        .unwrap();
        (pool, tables)
    }

    let (_reg_pool, reg_tables) = apply_and_list(&reg_root, &reg_dir).await;
    let (ten_pool, ten_tables) = apply_and_list(&ten_root, &ten_dir).await;

    let has = |v: &[String], t: &str| v.iter().any(|x| x == t);

    // Registry owns orgs + operators; never the tenant-user tables.
    assert!(has(&reg_tables, "rustango_orgs"), "reg: {reg_tables:?}");
    assert!(
        has(&reg_tables, "rustango_operators"),
        "reg: {reg_tables:?}"
    );
    assert!(
        !has(&reg_tables, "rustango_users"),
        "reg must not own users: {reg_tables:?}"
    );

    // Tenant owns the user/role/permission cluster.
    for t in [
        "rustango_users",
        "rustango_roles",
        "rustango_permissions",
        "rustango_role_permissions",
        "rustango_user_roles",
        "rustango_user_permissions",
    ] {
        assert!(has(&ten_tables, t), "tenant should own {t}: {ten_tables:?}");
    }
    assert!(
        !has(&ten_tables, "rustango_orgs"),
        "tenant must not own orgs: {ten_tables:?}"
    );

    // Per-DB shared tables land in BOTH scopes (one copy per database).
    for shared in ["rustango_audit_log", "rustango_content_types"] {
        assert!(
            has(&reg_tables, shared),
            "reg missing shared {shared}: {reg_tables:?}"
        );
        assert!(
            has(&ten_tables, shared),
            "tenant missing shared {shared}: {ten_tables:?}"
        );
    }

    // The composite unique index on the permissions table came through
    // from the model's `unique_together` (tenant DB).
    let Pool::Sqlite(sq) = &ten_pool else {
        unreachable!()
    };
    let idx: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='rustango_permissions_table_name_codename_idx'",
    ).fetch_one(sq).await.unwrap();
    assert_eq!(idx, 1, "composite unique index must exist");

    // Re-run is idempotent (ledger tracks applied).
    assert!(rustango::migrate::migrate_pool(&ten_pool, &ten_dir)
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
