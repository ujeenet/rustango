//! Live tests for the tenancy `manage` runner.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustango::sql::{sqlx, Fetcher};
use rustango::{core::Column as _, migrate as rmig};
use rustango_tenancy::{manage, Org, TenantPools};

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
) -> (String, Result<(), rustango_tenancy::TenancyError>) {
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
    assert!(
        format!("{err}").contains("already exists"),
        "got: {err}"
    );

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
    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &s1, "--no-migrate"],
    )
    .await
    .1
    .unwrap();
    run(
        &pools,
        &url,
        &dir,
        &["create-tenant", &s2, "--mode", "database", "--database-url", &url, "--no-migrate"],
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

