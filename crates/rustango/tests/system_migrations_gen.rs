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
        let sq = pool.as_sqlite().expect("sqlite pool");
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
    let sq = ten_pool.as_sqlite().expect("sqlite pool");
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
fn admin_sso_feature_toggles_provider_table() {
    use rustango::migrate::{detect_changes, SchemaSnapshot};
    // v0.47 — SSO config moved off the `Org` row into dedicated models.
    // Enabling `admin-sso` now adds the registry-scoped
    // `rustango_shared_sso_providers` table (its per-tenant twin
    // `rustango_sso_providers` toggles in the tenant scope the same way).
    let with_sso = SchemaSnapshot::from_registry_system_for_scope(ModelScope::Registry);
    assert!(
        with_sso
            .tables
            .iter()
            .any(|t| t.name == "rustango_shared_sso_providers"),
        "admin-sso registry snapshot must include rustango_shared_sso_providers"
    );
    // Simulate the feature OFF by dropping that table from the snapshot.
    let mut without_sso = with_sso.clone();
    without_sso
        .tables
        .retain(|t| t.name != "rustango_shared_sso_providers");
    assert_ne!(with_sso, without_sso);

    // Enabling (off → on) generates a CreateTable; disabling a DropTable.
    let enable = format!("{:?}", detect_changes(&without_sso, &with_sso));
    let disable = format!("{:?}", detect_changes(&with_sso, &without_sso));
    eprintln!("ENABLE ops: {enable}");
    eprintln!("DISABLE ops: {disable}");
    assert!(
        enable.contains("CreateTable(\"rustango_shared_sso_providers\")"),
        "enabling admin-sso must CreateTable the shared providers: {enable}"
    );
    assert!(
        disable.contains("DropTable(\"rustango_shared_sso_providers\")"),
        "disabling admin-sso must DropTable the shared providers: {disable}"
    );
}

/// `true` when `table` has `column` in the SQLite database behind `pool`.
async fn has_column(pool: &rustango::sql::Pool, table: &str, column: &str) -> bool {
    let sq = pool.as_sqlite().expect("sqlite pool");
    let n: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?",
    )
    .bind(table)
    .bind(column)
    .fetch_one(sq)
    .await
    .unwrap();
    n == 1
}

/// `true` when `table` exists in the SQLite database behind `pool`.
async fn has_table(pool: &rustango::sql::Pool, table: &str) -> bool {
    has_column(pool, table, "id").await
}

/// #1988 — an image without `system/` regenerates a baseline whose name
/// is already in the ledger. A framework table the upgrade adds is
/// modelled by dropping one; a second dir must restore it, not skip it.
#[tokio::test]
async fn single_db_migrate_from_a_dir_without_system_restores_the_schema() {
    use rustango::sql::Pool;
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("db.sqlite").display());
    let pool = Pool::connect(&url).await.unwrap();
    let migrate = |dir: std::path::PathBuf| {
        let pool = pool.clone();
        async move {
            std::fs::create_dir_all(&dir).unwrap();
            let mut out = Vec::new();
            rustango::migrate::manage::run_with_writer(
                &pool,
                &dir,
                ["migrate".to_owned()],
                &mut out,
            )
            .await
        }
    };
    migrate(tmp.path().join("deploy1/migrations"))
        .await
        .expect("first deploy");
    let table = "rustango_user_permissions";
    rustango::sql::raw_execute_pool(&pool, &format!("DROP TABLE {table}"), Vec::new())
        .await
        .unwrap();

    migrate(tmp.path().join("deploy2/migrations"))
        .await
        .expect("a second dir migrates the same database");
    assert!(has_table(&pool, table).await, "{table} was skipped");
}

/// The tenancy runner takes the same path; a dropped column comes back too.
#[tokio::test]
async fn registry_migrate_from_a_dir_without_system_restores_the_schema() {
    use rustango::sql::Pool;
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = Pool::connect(&url).await.unwrap();
    let migrate = |dir: std::path::PathBuf| {
        let pool = pool.clone();
        async move {
            std::fs::create_dir_all(&dir).unwrap();
            rustango::tenancy::migrate_registry_pool(&pool, &dir).await
        }
    };
    migrate(tmp.path().join("deploy1/migrations"))
        .await
        .expect("first deploy");
    let (table, column) = ("rustango_operators", "password_changed_at");
    rustango::sql::raw_execute_pool(
        &pool,
        &format!("ALTER TABLE {table} DROP COLUMN {column}"),
        Vec::new(),
    )
    .await
    .unwrap();

    migrate(tmp.path().join("deploy2/migrations"))
        .await
        .expect("a second dir migrates the same database");
    assert!(
        has_column(&pool, table, column).await,
        "{table}.{column} was skipped"
    );
}

/// `manage migrate` against `pool` from `dir`, which it creates.
async fn manage_migrate(
    pool: &rustango::sql::Pool,
    dir: std::path::PathBuf,
) -> Result<(), rustango::migrate::MigrateError> {
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = Vec::new();
    rustango::migrate::manage::run_with_writer(pool, &dir, ["migrate".to_owned()], &mut out).await
}

async fn exec(pool: &rustango::sql::Pool, sql: &str) {
    rustango::sql::raw_execute_pool(pool, sql, Vec::new())
        .await
        .unwrap();
}

/// A `database`-mode SQLite tenant whose file is `<dir>/<slug>.db`.
fn sqlite_org(dir: &std::path::Path, slug: &str) -> rustango::tenancy::Org {
    rustango::tenancy::Org {
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(format!(
            "sqlite:{}?mode=rwc",
            dir.join(format!("{slug}.db")).display()
        )),
        ..rustango::testkit::org()
    }
}

/// Migrate the registry, then every tenant, from `dir`.
async fn deploy(
    registry: &rustango::sql::Pool,
    pools: &rustango::tenancy::TenantPools<rustango::sql::sqlx::Sqlite>,
    dir: &std::path::Path,
) {
    std::fs::create_dir_all(dir).unwrap();
    rustango::tenancy::migrate_registry_pool(registry, dir)
        .await
        .expect("registry");
    let report = rustango::tenancy::migrate_tenants_db(pools, dir, "")
        .await
        .expect("tenants");
    assert!(report.all_ok(), "{report:?}");
}

/// #1988 — the registry run fills a fresh dir first; every later tenant
/// must still converge, so a table and column dropped in the 2nd come back.
#[tokio::test]
async fn tenants_after_the_registry_restore_the_schema() {
    use rustango::sql::Pool;
    let tmp = tempfile::tempdir().unwrap();
    let reg_url = format!("sqlite:{}?mode=rwc", tmp.path().join("reg.db").display());
    let reg = rustango::sql::sqlx::SqlitePool::connect(&reg_url)
        .await
        .unwrap();
    let pools = rustango::tenancy::TenantPools::new(reg.clone());
    let registry = Pool::Sqlite(reg);
    let boot = tmp.path().join("boot/migrations");
    std::fs::create_dir_all(&boot).unwrap();
    rustango::tenancy::migrate_registry_pool(&registry, &boot)
        .await
        .expect("registry tables");
    for slug in ["t1", "t2", "t3"] {
        let mut org = sqlite_org(tmp.path(), slug);
        org.insert_pool(&registry).await.unwrap();
    }
    deploy(&registry, &pools, &tmp.path().join("deploy1/migrations")).await;
    let t2 = Pool::connect(&sqlite_org(tmp.path(), "t2").database_url.unwrap())
        .await
        .unwrap();
    exec(&t2, "DROP TABLE rustango_user_permissions").await;
    exec(
        &t2,
        "ALTER TABLE rustango_users DROP COLUMN password_changed_at",
    )
    .await;

    deploy(&registry, &pools, &tmp.path().join("deploy2/migrations")).await;
    assert!(
        has_table(&t2, "rustango_user_permissions").await,
        "the 2nd tenant's dropped table was skipped"
    );
    assert!(
        has_column(&t2, "rustango_users", "password_changed_at").await,
        "the 2nd tenant's dropped column was skipped"
    );
}

/// SQLite can't `ADD COLUMN … DEFAULT (strftime(…'now'))` on a table with
/// rows; converge must still restore such a column instead of failing.
#[tokio::test]
async fn converge_adds_a_now_default_column_to_a_table_with_rows() {
    use rustango::sql::Pool;
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("db.sqlite").display());
    let pool = Pool::connect(&url).await.unwrap();
    manage_migrate(&pool, tmp.path().join("deploy1/migrations"))
        .await
        .expect("first deploy");
    exec(
        &pool,
        "INSERT INTO rustango_audit_log (entity_table, entity_pk, operation, source, changes) \
         VALUES ('t', '1', 'create', 'test', '{}')",
    )
    .await;
    let sq = pool.as_sqlite().unwrap();
    let indexes: Vec<String> = rustango::sql::sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' \
         AND tbl_name = 'rustango_audit_log' AND sql LIKE '%occurred_at%'",
    )
    .fetch_all(sq)
    .await
    .unwrap();
    for ix in indexes {
        exec(&pool, &format!("DROP INDEX \"{ix}\"")).await;
    }
    exec(
        &pool,
        "ALTER TABLE rustango_audit_log DROP COLUMN occurred_at",
    )
    .await;

    manage_migrate(&pool, tmp.path().join("deploy2/migrations"))
        .await
        .expect("a now() column on a table with rows must not fail the boot");
    assert!(has_column(&pool, "rustango_audit_log", "occurred_at").await);
    let null_rows: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT COUNT(*) FROM rustango_audit_log WHERE occurred_at IS NULL",
    )
    .fetch_one(sq)
    .await
    .unwrap();
    assert_eq!(null_rows, 0, "existing rows got no timestamp");
}

/// One column converge can't add must not block the rest, and the error
/// must name it without telling the user to delete framework files.
#[tokio::test]
async fn converge_reports_what_it_cannot_add_and_fixes_the_rest() {
    use rustango::sql::Pool;
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("db.sqlite").display());
    let pool = Pool::connect(&url).await.unwrap();
    manage_migrate(&pool, tmp.path().join("deploy1/migrations"))
        .await
        .expect("first deploy");
    exec(&pool, "ALTER TABLE rustango_audit_log DROP COLUMN source").await;
    exec(&pool, "DROP TABLE rustango_user_permissions").await;

    let err = manage_migrate(&pool, tmp.path().join("deploy2/migrations"))
        .await
        .expect_err("a NOT NULL column with no default can't be added")
        .to_string();
    assert!(
        has_table(&pool, "rustango_user_permissions").await,
        "one bad column blocked the other tables: {err}"
    );
    assert!(err.contains("rustango_audit_log.source"), "{err}");
    assert!(!err.to_lowercase().contains("delete"), "{err}");
}

/// Mixed-scope project migrations: tenants must apply the committed
/// `system/migrations/`, not a chain regenerated in a temp dir.
#[tokio::test]
async fn mixed_scope_tenants_use_the_committed_system_chain() {
    use rustango::migrate::{Migration, MigrationScope, Operation, SchemaChange, SchemaSnapshot};
    use rustango::sql::Pool;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("app");
    let dir = root.join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, scope, table) in [
        ("0001_reg", MigrationScope::Registry, "reg_things"),
        ("0002_ten", MigrationScope::Tenant, "ten_things"),
    ] {
        let table: rustango::migrate::TableSnapshot = serde_json::from_value(serde_json::json!({
            "name": table, "model": "T",
            "fields": [{"name": "id", "column": "id", "ty": "i64",
                        "nullable": false, "primary_key": true}]
        }))
        .unwrap();
        let mig = Migration {
            name: name.into(),
            created_at: "2026-10-01T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope,
            replaces: Vec::new(),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(
                table.name.clone(),
            ))],
            snapshot: SchemaSnapshot {
                tables: vec![table],
                ..Default::default()
            },
        };
        rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();
    }
    rustango::migrate::make_migrations_system(&root, ModelScope::Tenant, Some("committed"))
        .unwrap()
        .expect("committed tenant chain");

    let reg_url = format!("sqlite:{}?mode=rwc", tmp.path().join("reg.db").display());
    let reg = rustango::sql::sqlx::SqlitePool::connect(&reg_url)
        .await
        .unwrap();
    let pools = rustango::tenancy::TenantPools::new(reg.clone());
    let registry = Pool::Sqlite(reg);
    rustango::tenancy::migrate_registry_pool(&registry, &dir)
        .await
        .expect("registry");
    let mut org = sqlite_org(tmp.path(), "t1");
    org.insert_pool(&registry).await.unwrap();
    let report = rustango::tenancy::migrate_tenants_db(&pools, &dir, "")
        .await
        .unwrap();
    assert!(report.all_ok(), "{report:?}");

    let tenant = Pool::connect(&org.database_url.unwrap()).await.unwrap();
    let n: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT COUNT(*) FROM __rustango_system_migrations__ WHERE name = '0001_committed'",
    )
    .fetch_one(tenant.as_sqlite().unwrap())
    .await
    .unwrap();
    assert_eq!(
        n, 1,
        "the tenant ran a regenerated chain, not the committed one"
    );
}

/// A framework change the engine can't emit fails `migrate`, rather than
/// applying the stale chain (#2014).
#[tokio::test]
async fn migrate_fails_when_the_system_chain_cannot_be_generated() {
    use rustango::sql::Pool;
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("db.sqlite").display());
    let pool = Pool::connect(&url).await.unwrap();
    let dir = tmp.path().join("deploy/migrations");
    manage_migrate(&pool, dir.clone())
        .await
        .expect("first deploy");
    // A `min` on a committed column: no op can move a live table there.
    let sys = tmp.path().join("deploy/system/migrations");
    let last = rustango::migrate::file::list_dir(&sys)
        .unwrap()
        .into_iter()
        .rev()
        .find(|m| m.scope == rustango::migrate::MigrationScope::Tenant)
        .expect("a tenant system migration");
    let path = sys.join(format!("{}.json", last.name));
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let field = json["snapshot"]["tables"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|t| t["name"] == "rustango_users")
        .and_then(|t| {
            t["fields"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|f| f["column"] == "username")
        })
        .expect("rustango_users.username");
    field["min"] = serde_json::json!(7);
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

    let err = manage_migrate(&pool, dir)
        .await
        .expect_err("a generation error must fail migrate")
        .to_string();
    assert!(err.contains("rustango_users.username"), "{err}");
}
