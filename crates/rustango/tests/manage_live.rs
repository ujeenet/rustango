#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! Live tests for the tenancy `manage` runner.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustango::sql::{sqlx, Fetcher};
use rustango::tenancy::{manage, Org, TenantPools};
use rustango::{core::Column as _, migrate as rmig};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
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
        .fetch(&pool)
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
    let err = res.unwrap_err();
    assert!(
        format!("{err}").contains("--database-url"),
        "expected database_url validation error, got: {err}"
    );

    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_tenant_rejects_duplicate_slug() {
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
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("drop_me");
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
        .fetch(&pool)
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
            &url,
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

#[tokio::test]
async fn unrecognized_subcommand_delegates_to_migrate_manage() {
    // `showmigrations` is a rustango_migrate verb, NOT tenancy. The
    // dispatcher must delegate gracefully.
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
    let mig_name = unique("0001_thing");
    let mig = rmig::Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rmig::MigrationScope::Tenant,
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

/// `purge-tenant` hard-deletes a schema-mode tenant: drops the
/// schema CASCADE, removes the Org row, prints a confirmation. Soft-
/// deleted (inactive) orgs purge cleanly too.
#[tokio::test]
async fn purge_tenant_schema_mode_drops_schema_and_org_row() {
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

    let (out, res) = run(
        &pools,
        &url,
        &dir,
        &["purge-tenant", &slug, "--confirm", &slug],
    )
    .await;
    res.unwrap();
    assert!(out.contains("purged"), "{out}");
    assert!(out.contains(&slug), "{out}");

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

/// End-to-end lifecycle: `init-tenancy` writes packaged bootstrap
/// migrations, `migrate` applies the registry-scoped one,
/// `create-operator` lands in `rustango_operators`, `create-tenant`
/// runs the tenant-scoped bootstrap so `rustango_users` exists in the
/// new schema automatically, and `create-user` writes into it.
#[tokio::test]
async fn full_provision_lifecycle_via_init_tenancy_and_migrate() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();

    // Clean slate: drop registry tables AND the migration ledger so
    // bootstrap migrations re-apply on this run.
    rmig::drop_all(&pool).await.unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "__rustango_migrations__" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();

    let dir = fresh_dir("lifecycle");
    let pools = TenantPools::new(pool.clone());

    // 1. init-tenancy writes the two packaged bootstraps.
    let (out, res) = run(&pools, &url, &dir, &["init-tenancy"]).await;
    res.unwrap();
    assert!(
        out.contains("0001_rustango_registry_initial"),
        "init-tenancy should mention registry bootstrap, got: {out}"
    );
    assert!(
        out.contains("0001_rustango_tenant_initial"),
        "init-tenancy should mention tenant bootstrap, got: {out}"
    );
    assert!(dir.join("0001_rustango_registry_initial.json").exists());
    assert!(dir.join("0001_rustango_tenant_initial.json").exists());

    // Re-running is idempotent — both files are reported as skipped.
    let (out2, res2) = run(&pools, &url, &dir, &["init-tenancy"]).await;
    res2.unwrap();
    assert!(
        out2.contains("already exists"),
        "second run should skip: {out2}"
    );

    // 2. Scope-aware `migrate` applies registry bootstrap (and runs
    //    the tenant phase, which is a no-op pre-tenants).
    let (out3, res3) = run(&pools, &url, &dir, &["migrate"]).await;
    res3.unwrap();
    assert!(
        out3.contains("0001_rustango_registry_initial"),
        "migrate should report the registry bootstrap, got: {out3}"
    );

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
    sqlx::query(r#"DROP TABLE IF EXISTS "__rustango_migrations__" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
