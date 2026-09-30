#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Tenancy CLI verbs honour their arguments (#1909, #1910).
//!
//! Each case once ran the real action with the flag dropped: a
//! `--dry-run` that migrated, a `migrate <target>` that put tenant tables
//! in the registry, a flag taken as a username.

use rustango::migrate::{file, Migration, MigrationScope, Operation, SchemaChange};
use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;

struct Booted {
    pools: TenantPools<sqlx::Sqlite>,
    raw: sqlx::SqlitePool,
    url: String,
    _tmp: tempfile::TempDir,
    migrations: tempfile::TempDir,
}

fn migration(name: &str, table: &str, scope: MigrationScope) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-09-30T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope,
        replaces: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": table, "model": "T",
                "fields": [{"name": "id", "column": "id", "ty": "i64",
                            "nullable": false, "primary_key": true}]
            }]
        }))
        .unwrap(),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.into()))],
    }
}

/// A registry with nothing applied, over a dir holding one registry-scoped
/// and one tenant-scoped migration.
async fn boot() -> Booted {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let raw = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = TenantPools::<sqlx::Sqlite>::new(raw.clone());
    let migrations = tempfile::tempdir().expect("migrations dir");
    for m in [
        migration("0001_reg", "reg_t", MigrationScope::Registry),
        migration("0002_ten", "ten_t", MigrationScope::Tenant),
    ] {
        file::write(&migrations.path().join(format!("{}.json", m.name)), &m).unwrap();
    }
    Booted {
        pools,
        raw,
        url,
        _tmp: tmp,
        migrations,
    }
}

impl Booted {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut buf: Vec<u8> = Vec::new();
        rustango::tenancy::manage::run_with_writer(
            &self.pools,
            &self.url,
            self.migrations.path(),
            args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            &mut buf,
        )
        .await
        .map(|()| String::from_utf8_lossy(&buf).into_owned())
        .map_err(|e| e.to_string())
    }

    async fn has_table(&self, name: &str) -> bool {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(name)
        .fetch_one(&self.raw)
        .await
        .expect("sqlite_master");
        n > 0
    }
}

#[tokio::test]
async fn migrate_registry_dry_run_and_help_apply_nothing() {
    let b = boot().await;
    let out = b
        .run(&["migrate-registry", "--dry-run"])
        .await
        .expect("dry-run");
    assert!(!b.has_table("reg_t").await, "--dry-run migrated: {out}");
    assert!(
        out.contains("0001_reg"),
        "should preview the pending one: {out}"
    );

    assert!(b.run(&["migrate-registry", "--help"]).await.is_err());
    assert!(b.run(&["migrate-registry", "--bogus"]).await.is_err());
    assert!(!b.has_table("reg_t").await, "--help / --bogus migrated");
}

#[tokio::test]
async fn migrate_tenants_refuses_flags_it_does_not_have() {
    let b = boot().await;
    b.run(&["migrate-registry"]).await.expect("registry");
    for flag in ["--dry-run", "--help", "--bogus"] {
        let err = b.run(&["migrate-tenants", flag]).await.expect_err(flag);
        assert!(err.contains("migrate-tenants"), "{flag}: {err}");
    }
}

#[tokio::test]
async fn migrate_target_and_dry_run_stay_registry_scoped() {
    let b = boot().await;
    let preview = b.run(&["migrate", "--dry-run"]).await.expect("dry-run");
    assert!(
        !preview.contains("0002_ten"),
        "tenant migration listed as pending on the registry: {preview}"
    );

    let _ = b.run(&["migrate", "0002_ten"]).await;
    assert!(
        !b.has_table("ten_t").await,
        "`migrate 0002_ten` created a tenant table in the registry"
    );

    b.run(&["migrate", "0001_reg"])
        .await
        .expect("registry target");
    assert!(b.has_table("reg_t").await);
}
