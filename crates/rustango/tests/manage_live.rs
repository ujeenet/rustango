#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! Live tests for the tenancy `manage` runner.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustango::sql::sqlx;
use rustango::tenancy::{manage, Org, TenantPools};
use rustango::{core::Column as _, migrate as rmig};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Joined with hyphens, because nearly every caller here is naming a
/// tenant slug — and a slug is a hostname label, where `_` is illegal.
fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}-{pid}-{n}")
}

/// Underscore-joined, for the one caller naming a *migration* rather
/// than a tenant: the name becomes a filename and a ledger entry, and
/// migration names are conventionally `0001_snake_case`.
fn unique_migration_name(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

/// The same server and credentials, a different database name.
///
/// Keeps userinfo, host and port; replaces only the path segment, so a
/// tenant lands on the server the test already reached.
fn sibling_database_url(registry_url: &str, database: &str) -> String {
    let (scheme, rest) = registry_url
        .split_once("://")
        .expect("DATABASE_URL should be a URL");
    let (authority, _old_db) = rest
        .rsplit_once('/')
        .expect("DATABASE_URL should name a database");
    format!("{scheme}://{authority}/{database}")
}

use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets the shared PG
/// schema; under cargo's default parallel harness two tests would race
/// on PG's `pg_type_typname_nsp_index` / `pg_class_relname_nsp_index`
/// system-catalog uniques when both try to CREATE/DROP the same table
/// at once.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

fn fresh_dir(label: &str) -> PathBuf {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_tenancy_manage_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn drop_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

fn args_vec(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

async fn run(
    pools: &TenantPools,
    url: &str,
    dir: &std::path::Path,
    parts: &[&str],
) -> (String, Result<(), rustango::tenancy::TenancyError>) {
    let mut buf: Vec<u8> = Vec::new();
    let res = manage::run_with_writer(pools, url, dir, args_vec(parts), &mut buf).await;
    (String::from_utf8_lossy(&buf).into_owned(), res)
}

#[tokio::test]
async fn create_tenant_inserts_row_creates_schema_and_runs_migrations() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("acme");
    drop_schema(&pool, &slug).await;

    let dir = fresh_dir("create");
    let pools = TenantPools::new(pool.clone());

    let (out, res) = run(
        &pools,
        &url,
        &dir,
        &[
            "create-tenant",
            &slug,
            "--mode",
            "schema",
            "--display-name",
            "ACME Corp",
            "--host-pattern",
            "acme.app.test",
            "--no-migrate",
        ],
    )
    .await;
    res.unwrap();
    assert!(out.contains("created tenant"), "{out}");
    assert!(out.contains(&slug), "{out}");
    assert!(out.contains("--no-migrate"), "{out}");

    // Org row landed.
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display_name, "ACME Corp");
    assert_eq!(rows[0].schema_name.as_deref(), Some(slug.as_str()));
    assert!(rows[0].active);

    // Schema exists.
    let exists: bool = sqlx::query_as::<_, (bool,)>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert!(exists, "schema `{slug}` should exist");

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_tenant_database_mode_requires_database_url() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("nodb");
    let dir = fresh_dir("create_nodb");
    let pools = TenantPools::new(pool.clone());

    let (_, res) = run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--mode", "database"],
    )
    .await;
    // The message no longer names `--database-url`: the check moved
    // into the provisioning engine, which the console and the webhook
    // share, and neither of those has a command-line flag to suggest.
    let err = res.unwrap_err();
    assert!(
        format!("{err}").contains("database mode needs a database URL"),
        "expected database_url validation error, got: {err}"
    );

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_tenant_rejects_duplicate_slug() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("dup");
    drop_schema(&pool, &slug).await;
    let dir = fresh_dir("create_dup");
    let pools = TenantPools::new(pool.clone());

    let (_, res) = run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--no-migrate"],
    )
    .await;
    res.unwrap();

    let (_, res) = run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--no-migrate"],
    )
    .await;
    let err = res.unwrap_err();
    assert!(format!("{err}").contains("already exists"), "got: {err}");

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn drop_tenant_soft_deletes_with_confirm() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("drop-me");
    drop_schema(&pool, &slug).await;
    let dir = fresh_dir("drop");
    let pools = TenantPools::new(pool.clone());

    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--no-migrate"],
    )
    .await
    .1
    .unwrap();

    // Without --confirm: error.
    let (_, res) = run(&pools, &url, &dir, &["drop-tenant", &slug]).await;
    assert!(res.is_err(), "drop-tenant without --confirm should fail");

    // Mismatched --confirm: error.
    let (_, res) = run(
        &pools,
        &url,
        &dir,
        &["drop-tenant", &slug, "--confirm", "wrong-slug"],
    )
    .await;
    assert!(
        format!("{}", res.unwrap_err()).contains("does not match"),
        "expected confirm-mismatch error"
    );

    // Correct --confirm: succeeds, soft-deletes.
    let (out, res) = run(
        &pools,
        &url,
        &dir,
        &["drop-tenant", &slug, "--confirm", &slug],
    )
    .await;
    res.unwrap();
    assert!(out.contains("soft-deleted"), "{out}");

    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "row preserved");
    assert!(!rows[0].active, "active flipped to false");

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn list_tenants_prints_all_orgs() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let dir = fresh_dir("list");
    let pools = TenantPools::new(pool.clone());

    // Empty list.
    let (out, res) = run(&pools, &url, &dir, &["list-tenants"]).await;
    res.unwrap();
    assert!(out.contains("(no tenants)"), "{out}");

    // Two tenants.
    let s1 = unique("alpha");
    let s2 = unique("beta");
    run(&pools, &url, &dir, &["create-tenant", &s1, "--no-migrate"])
        .await
        .1
        .unwrap();
    // A real second database, not `url`. Passing the registry's own
    // URL here is what provisioning refuses: it would run the tenant
    // migration chain over the registry. `CREATE DATABASE` has no
    // `IF NOT EXISTS` in Postgres, so a re-run's "already exists" is
    // swallowed; a genuine failure surfaces at the connection check.
    let tenant_db = "rustango_tenant_list_test";
    let _ = sqlx::query(&format!("CREATE DATABASE {tenant_db}"))
        .execute(&pool)
        .await;
    let tenant_url = sibling_database_url(&url, tenant_db);
    run(
        &pools,
        &url,
        &dir,
        &[
            "create-tenant",
            &s2,
            "--mode",
            "database",
            "--database-url",
            &tenant_url,
            "--no-migrate",
        ],
    )
    .await
    .1
    .unwrap();

    let (out, res) = run(&pools, &url, &dir, &["list-tenants"]).await;
    res.unwrap();
    assert!(out.contains(&s1), "alpha missing: {out}");
    assert!(out.contains(&s2), "beta missing: {out}");
    assert!(out.contains("schema"), "mode column missing: {out}");
    assert!(out.contains("database"), "database mode missing: {out}");

    drop_schema(&pool, &s1).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Database-mode PG tenants seed the reserved codenames too (#1933).
#[tokio::test]
async fn migrate_tenants_seeds_reserved_perms_in_a_database_mode_tenant() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();
    let tenant_db = "rustango_tenant_perm_test";
    sqlx::query(&format!("DROP DATABASE IF EXISTS {tenant_db} WITH (FORCE)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {tenant_db}"))
        .execute(&pool)
        .await
        .unwrap();
    let tenant_url = sibling_database_url(&url, tenant_db);
    let dir = fresh_dir("dbperm");
    let pools = TenantPools::new(pool.clone());
    let slug = unique("dbperm");
    let create = [
        "create-tenant",
        &slug,
        "--mode",
        "database",
        "--database-url",
        &tenant_url,
        "--no-migrate",
    ];
    run(&pools, &url, &dir, &create).await.1.unwrap();
    let (out, res) = run(&pools, &url, &dir, &["migrate-tenants"]).await;
    res.unwrap_or_else(|e| panic!("{e}: {out}"));

    let tenant = sqlx::PgPool::connect(&tenant_url).await.unwrap();
    let (seeded,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM rustango_permissions WHERE codename = 'auth.access_admin'",
    )
    .fetch_one(&tenant)
    .await
    .unwrap();
    assert_eq!(seeded, 1, "database-mode tenant lacks auth.access_admin");
    tenant.close().await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unrecognized_subcommand_delegates_to_migrate_manage() {
    // `showmigrations` is a rustango_migrate verb, NOT tenancy. The
    // dispatcher must delegate gracefully.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let dir = fresh_dir("delegate");
    let pools = TenantPools::new(pool.clone());

    let (out, res) = run(&pools, &url, &dir, &["showmigrations"]).await;
    res.unwrap();
    // showmigrations on an empty dir prints "(no migrations in <dir>)"
    assert!(out.contains("no migrations"), "{out}");

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_tenants_runs_against_active_only() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("active");
    drop_schema(&pool, &slug).await;
    let dir = fresh_dir("mig");
    let pools = TenantPools::new(pool.clone());

    // Seed an org via the manage path.
    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--no-migrate"],
    )
    .await
    .1
    .unwrap();

    // Ship a tenant migration in dir.
    let mig_name = unique_migration_name("0001_thing");
    let mig = rmig::Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rmig::MigrationScope::Tenant,
        replaces: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": "thing", "model": "T",
                "fields": [
                    {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
                ]
            }]
        }))
        .unwrap(),
        forward: vec![rmig::Operation::Schema(rmig::SchemaChange::CreateTable(
            "thing".into(),
        ))],
    };
    rmig::file::write(&dir.join(format!("{}.json", mig_name)), &mig).unwrap();

    let (out, res) = run(&pools, &url, &dir, &["migrate-tenants"]).await;
    res.unwrap();
    assert!(out.contains(&slug), "{out}");
    assert!(out.contains("migration"), "{out}");
    // #1320 — the verb reports each migration as it lands, not just a
    // summary once the whole run is over. `app/` distinguishes the
    // project's chain from the framework's `system/` one, which numbers
    // independently and would otherwise look like a repeat.
    assert!(
        out.contains(&format!("applied app/{mig_name}")),
        "expected per-migration progress for {mig_name}: {out}"
    );

    let exists: bool = sqlx::query_as::<_, (bool,)>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = 'thing')",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert!(exists, "tenant table should be created");

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new schema tenant applies a migration `public`'s ledger already lists:
/// it reads its own ledger, not `public`'s through the search path (#2143).
#[tokio::test]
async fn a_new_schema_tenant_does_not_read_publics_project_ledger() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("ledger");
    drop_schema(&pool, &slug).await;
    let dir = fresh_dir("ledger2143");
    let pools = TenantPools::new(pool.clone());
    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--no-migrate"],
    )
    .await
    .1
    .unwrap();

    let mig_name = unique_migration_name("0001_ledger");
    let mig = rmig::Migration {
        name: mig_name.clone(),
        created_at: "2026-10-02T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rmig::MigrationScope::Tenant,
        replaces: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": "ledger2143_thing", "model": "T",
                "fields": [
                    {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
                ]
            }]
        }))
        .unwrap(),
        forward: vec![rmig::Operation::Schema(rmig::SchemaChange::CreateTable(
            "ledger2143_thing".into(),
        ))],
    };
    rmig::file::write(&dir.join(format!("{mig_name}.json")), &mig).unwrap();
    // `public` ran this chain, e.g. as a tenant living in the default schema.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS public.__rustango_migrations__ \
         (name VARCHAR(255) PRIMARY KEY, applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO public.__rustango_migrations__ (name) VALUES ($1)")
        .bind(&mig_name)
        .execute(&pool)
        .await
        .unwrap();

    let (out, res) = run(&pools, &url, &dir, &["migrate-tenants"]).await;
    res.unwrap();
    let exists: bool = sqlx::query_as::<_, (bool,)>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = 'ledger2143_thing')",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert!(
        exists,
        "the tenant skipped a migration public applied: {out}"
    );

    sqlx::query("DELETE FROM public.__rustango_migrations__ WHERE name = $1")
        .bind(&mig_name)
        .execute(&pool)
        .await
        .unwrap();
    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `purge-tenant` hard-deletes a schema-mode tenant: drops the
/// schema CASCADE, removes the Org row, prints a confirmation. Soft-
/// deleted (inactive) orgs purge cleanly too.
#[tokio::test]
async fn purge_tenant_schema_mode_drops_schema_and_org_row() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("purgeme");
    drop_schema(&pool, &slug).await;

    let dir = fresh_dir("purge_schema");
    let pools = TenantPools::new(pool.clone());

    // Provision: skip migrations to keep the test focused on purge.
    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--mode", "schema", "--no-migrate"],
    )
    .await
    .1
    .unwrap();

    // Drop a marker table inside the schema so we can prove CASCADE
    // wiped it. (`rustango_users` doesn't exist without a migrate
    // run; this is a tiny direct table for the assertion.)
    let marker_sql = format!(r#"CREATE TABLE "{slug}"."widget" (id INT)"#);
    sqlx::query(&marker_sql).execute(&pool).await.unwrap();

    // Brand files in the default store go with the tenant (#1933).
    let brand = tempfile::tempdir().unwrap();
    std::env::set_var(
        rustango::tenancy::branding::BRAND_STORAGE_ROOT_ENV,
        brand.path(),
    );
    let brand_dir = brand.path().join(&slug);
    std::fs::create_dir_all(&brand_dir).unwrap();
    for f in ["logo.png", "favicon.ico"] {
        std::fs::write(brand_dir.join(f), b"x").unwrap();
    }

    let (out, res) = run(
        &pools,
        &url,
        &dir,
        &["purge-tenant", &slug, "--confirm", &slug],
    )
    .await;
    std::env::remove_var(rustango::tenancy::branding::BRAND_STORAGE_ROOT_ENV);
    res.unwrap();
    assert!(out.contains("purged"), "{out}");
    assert!(out.contains(&slug), "{out}");
    for f in ["logo.png", "favicon.ico"] {
        assert!(!brand_dir.join(f).exists(), "{f} survived the purge");
    }
    assert!(out.contains("custom brand store"), "{out}");

    // Schema gone (CASCADE took the marker table with it).
    let exists: bool = sqlx::query_as::<_, (bool,)>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert!(!exists, "schema `{slug}` should be gone");

    // Org row deleted.
    let row_count: i64 =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*)::bigint FROM rustango_orgs WHERE slug = $1")
            .bind(&slug)
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
    assert_eq!(row_count, 0);

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `purge-tenant` rejects `--confirm` mismatch loudly.
#[tokio::test]
async fn purge_tenant_rejects_confirm_mismatch() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("safe");
    drop_schema(&pool, &slug).await;
    let dir = fresh_dir("purge_mismatch");
    let pools = TenantPools::new(pool.clone());
    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--mode", "schema", "--no-migrate"],
    )
    .await
    .1
    .unwrap();

    let (_out, res) = run(
        &pools,
        &url,
        &dir,
        &["purge-tenant", &slug, "--confirm", "wrong-slug"],
    )
    .await;
    let err = res.unwrap_err().to_string();
    assert!(err.contains("does not match"), "{err}");

    // Tenant still here.
    let exists: bool = sqlx::query_as::<_, (bool,)>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert!(exists, "schema should still exist after rejected purge");

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `purge-tenant` for a database-mode org refuses without
/// `--purge-database`. The Org row + dedicated DB stay intact.
#[tokio::test]
async fn purge_tenant_database_mode_requires_purge_database_flag() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("dbmode");
    let dir = fresh_dir("purge_db_no_flag");
    let pools = TenantPools::new(pool.clone());

    // Insert a database-mode Org row directly (no schema to drop;
    // database_url points at the registry DB, which is degenerate
    // but exercises the refuse-without-flag path).
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO rustango_orgs (slug, display_name, storage_mode, database_url, \
         schema_name, host_pattern, port, path_prefix, active, created_at) \
         VALUES ($1, $1, 'database', $2, NULL, NULL, NULL, NULL, true, $3::timestamptz)",
    )
    .bind(&slug)
    .bind(&url)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    let (_out, res) = run(
        &pools,
        &url,
        &dir,
        &["purge-tenant", &slug, "--confirm", &slug],
    )
    .await;
    let err = res.unwrap_err().to_string();
    assert!(err.contains("--purge-database"), "{err}");
    assert!(err.contains("unrecoverable"), "{err}");

    // Org row still present.
    let row_count: i64 =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*)::bigint FROM rustango_orgs WHERE slug = $1")
            .bind(&slug)
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
    assert_eq!(row_count, 1);

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `purge-tenant` for an unknown slug errors clearly without
/// touching anything.
#[tokio::test]
async fn purge_tenant_unknown_slug_errors() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let dir = fresh_dir("purge_missing");
    let pools = TenantPools::new(pool.clone());
    let bogus = unique("ghost");
    let (_out, res) = run(
        &pools,
        &url,
        &dir,
        &["purge-tenant", &bogus, "--confirm", &bogus],
    )
    .await;
    let err = res.unwrap_err().to_string();
    assert!(err.contains("no tenant"), "{err}");

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `purge-tenant` works on a soft-deleted (inactive) org — hard-
/// delete is the right next step after `drop-tenant`.
#[tokio::test]
async fn purge_tenant_works_on_soft_deleted_org() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("ghost");
    drop_schema(&pool, &slug).await;
    let dir = fresh_dir("purge_softdeleted");
    let pools = TenantPools::new(pool.clone());

    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &slug, "--mode", "schema", "--no-migrate"],
    )
    .await
    .1
    .unwrap();
    run(
        &pools,
        &url,
        &dir,
        &["drop-tenant", &slug, "--confirm", &slug],
    )
    .await
    .1
    .unwrap();

    // Sanity: soft-deleted but still present.
    let active: bool =
        sqlx::query_as::<_, (bool,)>("SELECT active FROM rustango_orgs WHERE slug = $1")
            .bind(&slug)
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
    assert!(!active);

    let (_out, res) = run(
        &pools,
        &url,
        &dir,
        &["purge-tenant", &slug, "--confirm", &slug],
    )
    .await;
    res.unwrap();

    let row_count: i64 =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*)::bigint FROM rustango_orgs WHERE slug = $1")
            .bind(&slug)
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
    assert_eq!(row_count, 0);

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// End-to-end lifecycle: `migrate` generates the framework's
/// `system/migrations/` on demand from the compiled models and applies
/// the registry-scoped ones, `create-operator` lands in
/// `rustango_operators`, `create-tenant` applies the tenant-scoped
/// system migrations so `rustango_users` exists in the new schema
/// automatically, and `create-user` writes into it. There is no
/// `init-tenancy` file-writing step: the framework ships no hardcoded
/// bootstrap JSON — its schema flows through the same makemigrations/
/// migrate engine as user models.
#[tokio::test]
async fn full_provision_lifecycle_via_init_tenancy_and_migrate() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();

    // Clean slate: drop registry tables AND both migration ledgers so the
    // system migrations re-apply on this run.
    rmig::drop_all(&pool).await.unwrap();
    for ledger in ["__rustango_migrations__", "__rustango_system_migrations__"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{ledger}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }

    let dir = fresh_dir("lifecycle");
    let pools = TenantPools::new(pool.clone());

    // 1. Scope-aware `migrate` generates the registry-scope system
    //    migrations from the compiled models and applies them (the tenant
    //    phase is a no-op pre-tenants).
    let (_out, res) = run(&pools, &url, &dir, &["migrate"]).await;
    res.unwrap();

    // rustango_orgs and rustango_operators now exist in public schema.
    for table in ["rustango_orgs", "rustango_operators"] {
        let exists: bool = sqlx::query_as::<_, (bool,)>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = $1)",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap()
        .0;
        assert!(exists, "{table} should exist in registry after migrate");
    }

    // The UNIQUE constraint on rustango_orgs.slug landed via the
    // packaged DataOp.
    let unique_count: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*)::bigint FROM information_schema.table_constraints \
         WHERE table_name = 'rustango_orgs' AND constraint_name = 'rustango_orgs_slug_key' \
         AND constraint_type = 'UNIQUE'",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert_eq!(unique_count, 1, "rustango_orgs.slug UNIQUE missing");

    // 3. create-operator lands in rustango_operators.
    let op_user = unique("admin");
    let (_out, res) = run(
        &pools,
        &url,
        &dir,
        &["create-operator", &op_user, "--password", "letmein"],
    )
    .await;
    res.unwrap();

    // 4. create-tenant — without --no-migrate, the tenant bootstrap
    //    runs against the new schema and rustango_users gets created.
    let slug = unique("acme");
    drop_schema(&pool, &slug).await;
    let (out4, res4) = run(
        &pools,
        &url,
        &dir,
        &[
            "create-tenant",
            &slug,
            "--mode",
            "schema",
            "--display-name",
            "ACME Corp",
        ],
    )
    .await;
    res4.unwrap();
    assert!(
        out4.contains("applied 1 migration"),
        "create-tenant should apply tenant bootstrap, got: {out4}"
    );

    // 5. rustango_users exists in <slug> schema with UNIQUE on username.
    let users_exists: bool = sqlx::query_as::<_, (bool,)>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = 'rustango_users')",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert!(users_exists, "rustango_users should exist in {slug}");

    let users_unique: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*)::bigint FROM information_schema.table_constraints \
         WHERE table_schema = $1 AND table_name = 'rustango_users' \
         AND constraint_name = 'rustango_users_username_key' \
         AND constraint_type = 'UNIQUE'",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert_eq!(
        users_unique, 1,
        "rustango_users.username UNIQUE missing in {slug}"
    );

    // 6. create-user writes into the tenant schema.
    let (out5, res5) = run(
        &pools,
        &url,
        &dir,
        &[
            "create-user",
            &slug,
            "alice",
            "--password",
            "hunter2",
            "--superuser",
        ],
    )
    .await;
    res5.unwrap();
    assert!(out5.contains("alice"), "{out5}");

    let user_count: i64 = sqlx::query_as::<_, (i64,)>(&format!(
        r#"SELECT COUNT(*)::bigint FROM "{slug}"."rustango_users""#,
    ))
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert_eq!(user_count, 1);

    // 6b. set-superuser / reset-password update the schema-mode row (#1952).
    for parts in [
        &["set-superuser", &slug, "alice", "--off"][..],
        &["reset-password", &slug, "alice", "--password", "hunter3"],
    ] {
        let (_out, res) = run(&pools, &url, &dir, parts).await;
        res.unwrap();
    }
    let (is_super, hash): (bool, String) = sqlx::query_as(&format!(
        r#"SELECT is_superuser, password_hash FROM "{slug}"."rustango_users" WHERE username = 'alice'"#,
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!is_super, "set-superuser --off did not land");
    assert!(rustango::tenancy::password::verify("hunter3", &hash).unwrap());

    // 6c. flush --tenant with no filter clears the tenant schema (#2284).
    let (out, res) = run(&pools, &url, &dir, &["flush", "--tenant", &slug, "--yes"]).await;
    res.unwrap_or_else(|e| {
        panic!(
            "whole-tenant flush: {e}
{out}"
        )
    });
    let user_count: i64 = sqlx::query_as::<_, (i64,)>(&format!(
        r#"SELECT COUNT(*)::bigint FROM "{slug}"."rustango_users""#,
    ))
    .fetch_one(&pool)
    .await
    .unwrap()
    .0;
    assert_eq!(user_count, 0, "flush --tenant left the tenant's users");

    // 6d. A registry model is filtered out, and the org row survives.
    let (out, res) = run(
        &pools,
        &url,
        &dir,
        &["flush", "--tenant", &slug, "--yes", "--model", "Org"],
    )
    .await;
    res.unwrap();
    assert!(out.contains("no tables match"), "{out}");
    let org_left: i64 =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*)::bigint FROM rustango_orgs WHERE slug = $1")
            .bind(&slug)
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
    assert_eq!(
        org_left, 1,
        "flush --tenant --model Org touched the registry"
    );

    // 6e. A table missing from the tenant schema never falls through to `public`.
    let (_out, res) = run(
        &pools,
        &url,
        &dir,
        &["create-user", &slug, "bob", "--password", "hunter2"],
    )
    .await;
    res.unwrap();
    for sql in [
        // A copy with no referrers, so only the schema decides what TRUNCATE hits.
        "DROP TABLE IF EXISTS public.rustango_users CASCADE".to_owned(),
        format!(r#"CREATE TABLE public.rustango_users AS SELECT * FROM "{slug}"."rustango_users""#),
        format!(r#"DROP TABLE "{slug}"."rustango_users" CASCADE"#),
    ] {
        sqlx::query(&sql).execute(&pool).await.unwrap();
    }
    let (_out, res) = run(
        &pools,
        &url,
        &dir,
        &["flush", "--tenant", &slug, "--yes", "--model", "User"],
    )
    .await;
    let public_users: i64 =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*)::bigint FROM public.rustango_users")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
    sqlx::query("DROP TABLE public.rustango_users CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(public_users, 1, "flush --tenant truncated public's table");
    assert!(res.is_err(), "the tenant has no rustango_users table");

    // 7. Org row landed.
    let org_count: i64 =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*)::bigint FROM rustango_orgs WHERE slug = $1")
            .bind(&slug)
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
    assert_eq!(org_count, 1);

    // Cleanup.
    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    for ledger in ["__rustango_migrations__", "__rustango_system_migrations__"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{ledger}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two tenants never share a schema: a purge of one would drop the other (#2290).
#[tokio::test]
async fn create_tenant_refuses_a_schema_another_tenant_uses() {
    use rustango::sql::FetcherPool as _;
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();
    let dir = fresh_dir("schema_clash");
    let pools = TenantPools::new(pool.clone());

    // An explicit name, then a slug whose default is that name.
    let shared = unique("shared");
    let legacy = unique("legacy");
    drop_schema(&pool, &shared).await;
    run(
        &pools,
        &url,
        &dir,
        &[
            "create-tenant",
            &legacy,
            "--mode",
            "schema",
            "--schema-name",
            &shared,
            "--no-migrate",
        ],
    )
    .await
    .1
    .unwrap();
    let (_, res) = run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &shared, "--mode", "schema", "--no-migrate"],
    )
    .await;
    let err = res.expect_err("a slug defaulting to a used schema must be refused");
    assert!(err.to_string().contains("already used"), "{err}");

    // A NULL `schema_name` claims the slug.
    let null_slug = unique("nullschema");
    let mut org = Org {
        slug: null_slug.clone(),
        display_name: null_slug.clone(),
        storage_mode: "schema".into(),
        schema_name: None,
        ..rustango::testkit::org()
    };
    org.save_pool(&rustango::sql::Pool::from(pool.clone()))
        .await
        .unwrap();
    let other = unique("other");
    let (_, res) = run(
        &pools,
        &url,
        &dir,
        &[
            "create-tenant",
            &other,
            "--mode",
            "schema",
            "--schema-name",
            &null_slug,
            "--no-migrate",
        ],
    )
    .await;
    assert!(res.is_err(), "a slug-default schema must be refused too");

    let orgs: Vec<Org> = Org::objects()
        .fetch(&rustango::sql::Pool::from(pool.clone()))
        .await
        .unwrap();
    let mut slugs: Vec<_> = orgs.iter().map(|o| o.slug.clone()).collect();
    slugs.sort();
    let mut want = vec![legacy.clone(), null_slug.clone()];
    want.sort();
    assert_eq!(slugs, want, "a refused tenant left a row");

    drop_schema(&pool, &shared).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The console's create form refuses a schema another tenant uses (#2290).
#[tokio::test]
async fn console_create_refuses_a_schema_another_tenant_uses() {
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use rustango::sql::{Auto, FetcherPool as _};
    use rustango::tenancy::operator_console::{router_with_provisioning, SessionSecret};
    use rustango::tenancy::provision::Provisioner;
    use tower::ServiceExt;

    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();
    let dir = fresh_dir("console_schema_clash");
    let pools = std::sync::Arc::new(TenantPools::new(pool.clone()));
    let registry = pools.registry_pool();

    let shared = unique("cshared");
    drop_schema(&pool, &shared).await;
    let mut org = Org {
        slug: unique("clegacy"),
        display_name: "legacy".into(),
        storage_mode: "schema".into(),
        schema_name: Some(shared.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&registry).await.unwrap();

    let username = unique("op");
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash("letmein").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(&registry).await.unwrap();
    let provisioner = Provisioner::new(pools.clone(), url.clone(), &dir).erased();
    let app = router_with_provisioning(
        registry.clone(),
        pools.clone(),
        provisioner,
        SessionSecret::from_env_or_random(),
    );
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("username={username}&password=letmein")))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = login
        .headers()
        .get("set-cookie")
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/new")
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "slug={shared}&storage_mode=schema&backend_kind=postgres&no_migrate=1"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("already used by another tenant"), "{html}");
    let taken: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(shared.clone()))
        .fetch(&registry)
        .await
        .unwrap();
    assert!(
        taken.is_empty(),
        "the console created a tenant on a used schema"
    );

    drop_schema(&pool, &shared).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A schema is free when only a database-mode tenant has that slug, or a
/// schema tenant has it as slug but lives elsewhere (#2290).
#[tokio::test]
async fn create_tenant_allows_a_schema_no_tenant_lives_in() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();
    let dir = fresh_dir("schema_free");
    let pools = TenantPools::new(pool.clone());
    let registry = rustango::sql::Pool::from(pool.clone());

    let db_slug = unique("dbmode");
    let mut db_org = Org {
        slug: db_slug.clone(),
        display_name: db_slug.clone(),
        database_url: Some("postgres://example@db.example.com/x".into()),
        ..rustango::testkit::org()
    };
    db_org.save_pool(&registry).await.unwrap();
    let moved = unique("moved");
    let elsewhere = unique("elsewhere");
    let mut schema_org = Org {
        slug: moved.clone(),
        display_name: moved.clone(),
        storage_mode: "schema".into(),
        schema_name: Some(elsewhere.clone()),
        ..rustango::testkit::org()
    };
    schema_org.save_pool(&registry).await.unwrap();

    for taken_slug in [&db_slug, &moved] {
        let fresh = unique("fresh");
        drop_schema(&pool, taken_slug).await;
        let (out, res) = run(
            &pools,
            &url,
            &dir,
            &[
                "create-tenant",
                &fresh,
                "--mode",
                "schema",
                "--schema-name",
                taken_slug,
                "--no-migrate",
            ],
        )
        .await;
        res.unwrap_or_else(|e| panic!("schema `{taken_slug}` is free: {e} {out}"));
        drop_schema(&pool, taken_slug).await;
    }

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `check --deploy` reports a tenant whose database never answers, without
/// waiting out the pool's 30 s acquire timeout (#2359).
#[cfg(feature = "sso")]
#[tokio::test]
async fn check_deploy_does_not_wait_on_a_silent_tenant() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    // Accepts connections and never answers.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = silent.local_addr().unwrap().port();
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((s, _)) = silent.accept().await {
            open.push(s);
        }
    });
    let slug = unique("silent");
    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        database_url: Some(format!("postgres://u:p@127.0.0.1:{port}/x")),
        ..rustango::testkit::org()
    };
    org.save_pool(&rustango::sql::Pool::from(pool.clone()))
        .await
        .unwrap();

    let pools = TenantPools::new(pool.clone());
    let dir = fresh_dir("sso_silent");
    let start = std::time::Instant::now();
    let (out, _) = run(&pools, &url, &dir, &["check", "--deploy"]).await;
    held.abort();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    assert!(
        out.contains(&format!(
            "[sso] tenant `{slug}`: could not check SSO providers: timed out"
        )),
        "{out}"
    );
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `check --deploy` reads a schema-mode tenant in its own schema, never
/// `public` (#2359).
#[cfg(feature = "sso")]
#[tokio::test]
async fn check_deploy_reads_a_schema_mode_tenant_in_its_schema() {
    use rustango::sql::Auto;
    use rustango::sso::{link::create_link, resolve_by_slug, LinkSource, SsoProvider};
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();
    // A probe that looked in `public` would find no provider table there.
    sqlx::query("DROP TABLE IF EXISTS public.rustango_sso_providers CASCADE")
        .execute(&pool)
        .await
        .unwrap();

    let slug = unique("ssoschema");
    drop_schema(&pool, &slug).await;
    sqlx::query(&format!(r#"CREATE SCHEMA "{slug}""#))
        .execute(&pool)
        .await
        .unwrap();
    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        storage_mode: "schema".into(),
        schema_name: Some(slug.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&rustango::sql::Pool::from(pool.clone()))
        .await
        .unwrap();
    let pools = TenantPools::new(pool.clone());
    let tenant = pools.scoped_pool_dyn(&org).await.unwrap();
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&tenant)
        .await
        .unwrap();
    rustango::testkit::create_tables_for::<SsoProvider>(&tenant)
        .await
        .unwrap();
    rustango::sso::link::ensure_table(&tenant).await.unwrap();
    let mut user = rustango::tenancy::User {
        username: "ann".into(),
        email: Some("ann@example.com".into()),
        ..rustango::testkit::user()
    };
    user.insert_pool(&tenant).await.unwrap();
    SsoProvider {
        id: Auto::default(),
        slug: "corp".into(),
        label: "corp".into(),
        kind: "oidc".into(),
        issuer_url: Some("https://idp.example".into()),
        client_id: "cid".into(),
        client_secret: rustango::casts::Cast::new("s3cret".into()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link: false,
        created_at: Auto::default(),
        updated_at: Auto::default(),
    }
    .insert_pool(&tenant)
    .await
    .unwrap();

    let dir = fresh_dir("sso_schema");
    let line = format!("[sso] tenant `{slug}`: provider `corp` has allow_email_link off");
    let (out, _) = run(&pools, &url, &dir, &["check", "--deploy"]).await;
    assert!(out.contains(&line), "{out}");

    let corp = resolve_by_slug(&tenant, "corp", String::new())
        .await
        .unwrap()
        .unwrap();
    let uid = user.id.get().copied().unwrap();
    create_link(&tenant, &corp.key(LinkSource::Tenant), "sub-ann", uid)
        .await
        .unwrap();
    let (out, _) = run(&pools, &url, &dir, &["check", "--deploy"]).await;
    assert!(!out.contains(&line), "{out}");

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A schema-mode tenant whose schema is missing is reported, not checked
/// against `public` (#2359).
#[cfg(feature = "sso")]
#[tokio::test]
async fn check_deploy_skips_a_schema_mode_tenant_without_its_schema() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();
    let ghost = unique("ghost");
    drop_schema(&pool, &ghost).await;
    let mut org = Org {
        slug: ghost.clone(),
        display_name: ghost.clone(),
        storage_mode: "schema".into(),
        schema_name: Some(ghost.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&rustango::sql::Pool::from(pool.clone()))
        .await
        .unwrap();
    let pools = TenantPools::new(pool.clone());
    let dir = fresh_dir("sso_ghost");
    let (out, _) = run(&pools, &url, &dir, &["check", "--deploy"]).await;
    assert!(
        out.contains(&format!(
            "[sso] tenant `{ghost}`: could not check SSO providers: schema `{ghost}` is missing"
        )),
        "{out}"
    );
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
