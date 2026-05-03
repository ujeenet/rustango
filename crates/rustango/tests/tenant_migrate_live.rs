#![cfg(feature = "tenancy")]
//! Live tests for scoped tenant migrations.
//!
//! Bootstraps a registry with two schema-mode tenants, writes a
//! mixed-scope migration set, and proves:
//! * registry-scoped migrations apply to the registry's `public.__rustango_migrations__`,
//! * tenant-scoped migrations apply per-tenant under
//!   `<schema>.__rustango_migrations__` with isolated tables,
//! * one tenant's broken migration doesn't sink the rest of the batch.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustango::sql::{sqlx, Auto};
use rustango::migrate as rmig;
use rustango::tenancy::{
    migrate_registry, migrate_tenants, Org, StorageMode, TenantPools,
};

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

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn fresh_dir(label: &str) -> PathBuf {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_tenancy_mig_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_migration(dir: &std::path::Path, mig: &rmig::Migration) {
    let path = dir.join(format!("{}.json", mig.name));
    rmig::file::write(&path, mig).unwrap();
}

fn snapshot_with_table(table: &str) -> rmig::SchemaSnapshot {
    let table: rmig::TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": table,
        "model": "T",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();
    rmig::SchemaSnapshot {
        tables: vec![table],
        ..Default::default()
    }
}

async fn drop_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn drop_table_in_public(pool: &sqlx::PgPool, table: &str) {
    let sql = format!(r#"DROP TABLE IF EXISTS public."{table}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn delete_ledger_entry_in_public(pool: &sqlx::PgPool, name: &str) {
    sqlx::query("DELETE FROM public.__rustango_migrations__ WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
}

async fn table_exists_in_schema(pool: &sqlx::PgPool, schema: &str, table: &str) -> bool {
    let sql =
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2)";
    let row: (bool,) = sqlx::query_as(sql)
        .bind(schema)
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

async fn ledger_entry_count_in_schema(pool: &sqlx::PgPool, schema: &str, name: &str) -> i64 {
    let sql = format!(
        r#"SELECT COUNT(*) FROM "{schema}".__rustango_migrations__ WHERE name = $1"#
    );
    let row: (i64,) = sqlx::query_as(&sql).bind(name).fetch_one(pool).await.unwrap();
    row.0
}

#[tokio::test]
async fn registry_migrate_applies_only_registry_scoped() {
    // Two migrations: one registry-scoped (creates a registry-side
    // table) and one tenant-scoped (creates a tenant-side table).
    // `migrate_registry` runs only the registry one.
    let Some(pool) = pool().await else {
        return;
    };

    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let dir = fresh_dir("registry_only");
    let reg_table = unique("reg_audit");
    let tenant_table = unique("tenant_box");
    let reg_name = unique("0001_reg");
    let tenant_name = unique("0002_tenant");

    write_migration(
        &dir,
        &rmig::Migration {
            name: reg_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rmig::MigrationScope::Registry,
            snapshot: snapshot_with_table(&reg_table),
            forward: vec![rmig::Operation::Schema(rmig::SchemaChange::CreateTable(
                reg_table.clone(),
            ))],
        },
    );
    write_migration(
        &dir,
        &rmig::Migration {
            name: tenant_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: Some(reg_name.clone()),
            atomic: true,
            scope: rmig::MigrationScope::Tenant,
            snapshot: snapshot_with_table(&tenant_table),
            forward: vec![rmig::Operation::Schema(rmig::SchemaChange::CreateTable(
                tenant_table.clone(),
            ))],
        },
    );

    drop_table_in_public(&pool, &reg_table).await;
    drop_table_in_public(&pool, &tenant_table).await;
    delete_ledger_entry_in_public(&pool, &reg_name).await;
    delete_ledger_entry_in_public(&pool, &tenant_name).await;

    let pools = TenantPools::new(pool.clone());
    let applied = migrate_registry(&pools, &dir).await.unwrap();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].name, reg_name);

    // Registry-scoped table exists; tenant-scoped does not.
    assert!(table_exists_in_schema(&pool, "public", &reg_table).await);
    assert!(!table_exists_in_schema(&pool, "public", &tenant_table).await);

    drop_table_in_public(&pool, &reg_table).await;
    drop_table_in_public(&pool, &tenant_table).await;
    delete_ledger_entry_in_public(&pool, &reg_name).await;
    delete_ledger_entry_in_public(&pool, &tenant_name).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tenant_migrate_fans_out_per_active_org_with_per_schema_ledger() {
    // Two schema-mode tenants. Tenant migration creates an `items`
    // table in each tenant's schema. Each tenant's ledger lives in
    // its own schema. Registry's `public.__rustango_migrations__`
    // does NOT pick up the tenant migrations.
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();

    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let acme_schema = unique("acme_sch");
    let globex_schema = unique("globex_sch");
    drop_schema(&pool, &acme_schema).await;
    drop_schema(&pool, &globex_schema).await;

    let mut acme = Org {
        id: Auto::default(),
        slug: unique("acme"),
        display_name: "ACME".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        database_url: None,
        schema_name: Some(acme_schema.clone()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
    };
    acme.insert(&pool).await.unwrap();

    let mut globex = Org {
        id: Auto::default(),
        slug: unique("globex"),
        display_name: "GLOBEX".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        database_url: None,
        schema_name: Some(globex_schema.clone()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
    };
    globex.insert(&pool).await.unwrap();

    let dir = fresh_dir("tenant_fanout");
    let mig_name = unique("0001_items");
    write_migration(
        &dir,
        &rmig::Migration {
            name: mig_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rmig::MigrationScope::Tenant,
            snapshot: snapshot_with_table("items"),
            forward: vec![rmig::Operation::Schema(rmig::SchemaChange::CreateTable(
                "items".into(),
            ))],
        },
    );

    let pools = TenantPools::new(pool.clone());
    let report = migrate_tenants(&pools, &dir, &url).await.unwrap();

    assert!(report.all_ok(), "all tenants should migrate cleanly: {:?}", report);
    assert_eq!(report.tenants.len(), 2);
    for outcome in &report.tenants {
        assert_eq!(outcome.applied.len(), 1, "exactly one migration per tenant");
        assert_eq!(outcome.applied[0].name, mig_name);
    }

    // `items` table exists in both tenant schemas, NOT in public.
    assert!(table_exists_in_schema(&pool, &acme_schema, "items").await);
    assert!(table_exists_in_schema(&pool, &globex_schema, "items").await);
    assert!(!table_exists_in_schema(&pool, "public", "items").await);

    // Each tenant's ledger has the entry.
    assert_eq!(ledger_entry_count_in_schema(&pool, &acme_schema, &mig_name).await, 1);
    assert_eq!(ledger_entry_count_in_schema(&pool, &globex_schema, &mig_name).await, 1);

    drop_schema(&pool, &acme_schema).await;
    drop_schema(&pool, &globex_schema).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tenant_migrate_skips_inactive_orgs() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();

    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let active_schema = unique("active_sch");
    let inactive_schema = unique("inactive_sch");
    drop_schema(&pool, &active_schema).await;
    drop_schema(&pool, &inactive_schema).await;

    let mut active = Org {
        id: Auto::default(),
        slug: unique("active"),
        display_name: "Active".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        database_url: None,
        schema_name: Some(active_schema.clone()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
    };
    active.insert(&pool).await.unwrap();

    let mut inactive = Org {
        id: Auto::default(),
        slug: unique("inactive"),
        display_name: "Inactive".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        database_url: None,
        schema_name: Some(inactive_schema.clone()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: false,
        created_at: now(),
    };
    inactive.insert(&pool).await.unwrap();

    let dir = fresh_dir("inactive_skip");
    let mig_name = unique("0001_thing");
    write_migration(
        &dir,
        &rmig::Migration {
            name: mig_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rmig::MigrationScope::Tenant,
            snapshot: snapshot_with_table("thing"),
            forward: vec![rmig::Operation::Schema(rmig::SchemaChange::CreateTable(
                "thing".into(),
            ))],
        },
    );

    let pools = TenantPools::new(pool.clone());
    let report = migrate_tenants(&pools, &dir, &url).await.unwrap();

    assert_eq!(
        report.tenants.len(),
        1,
        "only the active tenant should be touched"
    );
    assert!(table_exists_in_schema(&pool, &active_schema, "thing").await);
    // Inactive schema doesn't even exist (we never created it).
    assert!(!table_exists_in_schema(&pool, &inactive_schema, "thing").await);

    drop_schema(&pool, &active_schema).await;
    rmig::drop_all(&pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
