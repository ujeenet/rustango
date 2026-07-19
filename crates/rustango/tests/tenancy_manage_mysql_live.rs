//! MySQL counterpart of `tenancy_manage_sqlite_live.rs`. Exercises
//! the full tenancy CLI cycle (`migrate-registry` → `create-tenant`
//! → `list-tenants`) against MySQL 8+.
//!
//! Pins the dialect-aware migration DDL emission contract on MySQL —
//! the generated system migration must emit `BIGINT NOT NULL AUTO_INCREMENT`
//! for `Auto<i64>` PKs (the MySQL equivalent of the SQLite
//! `INTEGER PRIMARY KEY AUTOINCREMENT` fix in slice 30).
//!
//! Skip-on-unset via `MYSQL_TEST_URL`.

#![cfg(all(feature = "mysql", feature = "tenancy"))]

use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;
use tokio::sync::Mutex;

/// Suite-wide lock. Both tests in this file `DROP TABLE` the shared
/// registry tables and then re-run `migrate-registry`; without
/// serialization the parallel harness races and trips MySQL error 1050
/// or partial-drop FK errors.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pools_or_skip() -> Option<(TenantPools<sqlx::MySql>, String)> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let pool = sqlx::MySqlPool::connect(&url)
        .await
        .expect("connect to MYSQL_TEST_URL");
    // Reset the registry side of the schema between runs.
    //
    // FK checks are disabled around the drops so prior CI steps that
    // left framework tables with FKs into ones we drop here (e.g.
    // `rustango_users` referenced by audit / permission tables in some
    // shape) don't make the drop fail silently. The per-table `let _ =`
    // swallows missing-table errors by design (first run starts empty),
    // so the SET statements must `expect` — otherwise the bypass
    // silently no-ops and the failure surfaces only later as a 42S01.
    //
    // The system-migration ledger is `__rustango_system_migrations__`
    // (the framework's own tables migrate under a dedicated ledger,
    // separate from the user-app `__rustango_migrations__`); if a stale
    // entry survives, `migrate-registry` thinks the system migrations are
    // already applied and skips creating the tables we just dropped. Both
    // ledgers are dropped below.
    sqlx::query("SET FOREIGN_KEY_CHECKS = 0")
        .execute(&pool)
        .await
        .expect("disable FK checks");
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
        "__rustango_migrations__",
        "__rustango_system_migrations__",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{tbl}`"))
            .execute(&pool)
            .await;
    }
    sqlx::query("SET FOREIGN_KEY_CHECKS = 1")
        .execute(&pool)
        .await
        .expect("re-enable FK checks");
    Some((TenantPools::<sqlx::MySql>::new(pool), url))
}

#[tokio::test]
async fn tenancy_manage_run_dispatches_init_then_create_then_list_on_mysql() {
    let _g = live_lock().lock().await;
    let Some((pools, url)) = pools_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let migrations_dir = tempfile::tempdir().expect("migrations tempdir");

    // Step 1: migrate-registry generates the framework's registry-scope
    // `system/migrations/` on demand from the compiled models and applies
    // them against MySQL. The runner must emit MySQL-flavored DDL
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
    // CAST to CHAR so sqlx decodes into String — `information_schema`'s
    // COLUMN_TYPE has a binary collation on MySQL 8, which sqlx surfaces
    // as `SQL type BLOB` and rejects against a Rust `String` target.
    let create_sql: String = sqlx::query_scalar::<_, String>(
        "SELECT CAST(COLUMN_TYPE AS CHAR) FROM information_schema.columns \
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
    let _g = live_lock().lock().await;
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
