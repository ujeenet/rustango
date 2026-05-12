//! MySQL counterpart of `tenancy_manage_sqlite_live.rs`. Exercises
//! the full tenancy CLI cycle (`init-tenancy` → `migrate-registry`
//! → `create-tenant` → `list-tenants`) against MySQL 8+.
//!
//! Pins the dialect-aware migration DDL emission contract on MySQL —
//! the bootstrap migration must emit `BIGINT NOT NULL AUTO_INCREMENT`
//! for `Auto<i64>` PKs (the MySQL equivalent of the SQLite
//! `INTEGER PRIMARY KEY AUTOINCREMENT` fix in slice 30).
//!
//! Skip-on-unset via `MYSQL_TEST_URL`.

#![cfg(all(feature = "mysql", feature = "tenancy"))]

use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;

async fn pools_or_skip() -> Option<(TenantPools<sqlx::MySql>, String)> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let pool = sqlx::MySqlPool::connect(&url)
        .await
        .expect("connect to MYSQL_TEST_URL");
    // Reset the registry side of the schema between runs. Drop in FK-
    // correct order so child rows / tables don't block the parent drop.
    for tbl in [
        "rustango_audit_log",
        "rustango_role_permissions",
        "rustango_user_permissions",
        "rustango_user_roles",
        "rustango_roles",
        "rustango_permissions",
        "rustango_content_types",
        "rustango_users",
        "rustango_operators",
        "rustango_orgs",
        "rustango_migrations",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{tbl}`"))
            .execute(&pool)
            .await;
    }
    Some((TenantPools::<sqlx::MySql>::new(pool), url))
}

#[tokio::test]
async fn tenancy_manage_run_dispatches_init_then_create_then_list_on_mysql() {
    let Some((pools, url)) = pools_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let migrations_dir = tempfile::tempdir().expect("migrations tempdir");

    // Step 1: init-tenancy writes the bootstrap migration JSONs.
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["init-tenancy".to_owned()],
        &mut buf,
    )
    .await
    .expect("init-tenancy");
    let registry_json = migrations_dir
        .path()
        .join("0001_rustango_registry_initial.json");
    assert!(registry_json.exists());

    // Step 2: migrate-registry — applies bootstrap migrations against
    // MySQL. The runner must emit MySQL-flavored DDL
    // (`BIGINT AUTO_INCREMENT PRIMARY KEY`, `DATETIME(6)`, `JSON`,
    // backtick identifiers) — not PG-flavored `BIGSERIAL` / `TIMESTAMPTZ`.
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    // Verify rustango_orgs exists. Pull its DDL via information_schema
    // and verify it carries MySQL-flavored types, not PG-only ones.
    let inner = pools.registry_inner();
    let create_sql: String = sqlx::query_scalar::<_, String>(
        "SELECT COLUMN_TYPE FROM information_schema.columns \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'rustango_orgs' AND COLUMN_NAME = 'id'",
    )
    .fetch_one(inner)
    .await
    .expect("rustango_orgs.id column type");
    assert!(
        create_sql.to_lowercase().contains("bigint"),
        "expected MySQL-flavored BIGINT for Auto<i64> PK, got: {create_sql}"
    );

    // Step 3: create-tenant — INSERT against the registry. The Auto<i64>
    // PK must auto-fill via MySQL's AUTO_INCREMENT (was the bug surfaced
    // on SQLite — MySQL bypass would manifest the same way if the runner
    // emitted PG-only `BIGSERIAL`).
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec![
            "create-tenant".to_owned(),
            "acme".to_owned(),
            "--display-name".to_owned(),
            "Acme Corp".to_owned(),
            "--mode".to_owned(),
            "database".to_owned(),
            "--database-url".to_owned(),
            // Reuse the same MySQL server for the tenant DB — point at
            // a parallel DATABASE name we'll create on the fly.
            url.clone(),
        ],
        &mut buf,
    )
    .await
    .expect("create-tenant");

    // Step 4: list-tenants should now show `acme`.
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["list-tenants".to_owned()],
        &mut buf,
    )
    .await
    .expect("list-tenants");
    let out = String::from_utf8_lossy(&buf);
    assert!(
        out.contains("acme"),
        "list-tenants should mention `acme`, got:\n{out}"
    );
}

#[tokio::test]
async fn tenancy_manage_run_rejects_schema_mode_on_mysql_with_friendly_error() {
    let Some((pools, url)) = pools_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let migrations_dir = tempfile::tempdir().expect("migrations tempdir");

    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["init-tenancy".to_owned()],
        &mut buf,
    )
    .await
    .expect("init-tenancy");
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    let mut buf: Vec<u8> = Vec::new();
    let err = rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec![
            "create-tenant".to_owned(),
            "schemamode_mysql".to_owned(),
            "--mode".to_owned(),
            "schema".to_owned(),
        ],
        &mut buf,
    )
    .await
    .expect_err("schema-mode on mysql should error");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("schema") || msg.contains("postgres"),
        "expected error to mention schema-mode / Postgres, got: {err}"
    );
}
