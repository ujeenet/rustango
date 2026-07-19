#![cfg(feature = "sqlite")]
//! Squash reconciliation (`Migration::replaces`) on the tri-dialect
//! `migrate_pool` runner, exercised against sqlite.
//!
//! Proves the three branches of `reconcile_action`:
//! * fresh DB → the squash RUNS (creates the tables);
//! * DB that already applied the replaced chain → the squash is
//!   FAKE-APPLIED (recorded, replaced rows tombstoned, no DDL, existing
//!   data preserved);
//! * DB that applied only part of the replaced chain → REFUSED.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{self, file, Migration, Operation, SchemaChange, SchemaSnapshot};
use rustango::sql::{sqlx, Pool};

static N: AtomicU32 = AtomicU32::new(0);

fn fresh_dir(label: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rustango_reconcile_{label}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn snapshot_with_table(table: &str) -> SchemaSnapshot {
    let t = serde_json::from_value(serde_json::json!({
        "name": table,
        "model": "T",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();
    SchemaSnapshot {
        tables: vec![t],
        ..Default::default()
    }
}

/// A create-table migration for `table`, optionally superseding `replaces`.
fn create_table_mig(name: &str, table: &str, replaces: &[&str]) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-07-19T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: migrate::MigrationScope::default(),
        replaces: replaces.iter().map(|s| (*s).to_owned()).collect(),
        snapshot: snapshot_with_table(table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(
            table.to_owned(),
        ))],
    }
}

fn write(dir: &Path, mig: &Migration) {
    std::fs::create_dir_all(dir).unwrap();
    file::write(&dir.join(format!("{}.json", mig.name)), mig).unwrap();
}

async fn pool_at(root: &Path) -> Pool {
    let url = format!("sqlite:{}?mode=rwc", root.join("db.sqlite").display());
    Pool::connect(&url).await.unwrap()
}

async fn ledger_names(pool: &Pool) -> Vec<String> {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query_scalar("SELECT name FROM __rustango_migrations__ ORDER BY name")
        .fetch_all(sq)
        .await
        .unwrap()
}

async fn table_exists(pool: &Pool, table: &str) -> bool {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    let c: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?")
            .bind(table)
            .fetch_one(sq)
            .await
            .unwrap();
    c == 1
}

/// Fresh DB: the squash has nothing to reconcile against, so it RUNS.
#[tokio::test]
async fn squash_on_fresh_db_runs_normally() {
    let root = fresh_dir("fresh");
    let dir = root.join("migrations");
    write(
        &dir,
        &create_table_mig("0001_squash", "rc_fresh", &["0001_a", "0002_b"]),
    );

    let pool = pool_at(&root).await;
    let applied = migrate::migrate_pool(&pool, &dir).await.unwrap();

    assert_eq!(applied.len(), 1);
    assert!(
        table_exists(&pool, "rc_fresh").await,
        "squash must create the table on a fresh DB"
    );
    assert_eq!(ledger_names(&pool).await, vec!["0001_squash".to_string()]);
}

/// DB that already applied the replaced chain: the squash is FAKE-APPLIED —
/// recorded, replaced rows tombstoned, NO CREATE TABLE re-run, data kept.
#[tokio::test]
async fn squash_on_migrated_db_fake_applies() {
    let root = fresh_dir("migrated");
    let pool = pool_at(&root).await;

    // Phase 1 — apply the old chain (two tables).
    let old = root.join("old");
    write(&old, &create_table_mig("0001_a", "rc_t1", &[]));
    write(&old, &create_table_mig("0002_b", "rc_t2", &[]));
    migrate::migrate_pool(&pool, &old).await.unwrap();
    // Put a row in t1 to prove data survives (no table re-creation).
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query("INSERT INTO rc_t1 (id) VALUES (42)")
            .execute(sq)
            .await
            .unwrap();
    }
    assert_eq!(
        ledger_names(&pool).await,
        vec!["0001_a".to_string(), "0002_b".to_string()]
    );

    // Phase 2 — the squashed dir replaces the old chain.
    let new = root.join("squashed");
    write(
        &new,
        &create_table_mig("0001_cms_initial", "rc_t1", &["0001_a", "0002_b"]),
    );
    let applied = migrate::migrate_pool(&pool, &new).await.unwrap();

    assert_eq!(applied.len(), 1, "the squash is recorded (fake-applied)");
    // Ledger: squash in, replaced names tombstoned out.
    assert_eq!(
        ledger_names(&pool).await,
        vec!["0001_cms_initial".to_string()]
    );
    // The table was NOT re-created (plain CREATE TABLE would have errored),
    // and the pre-existing row is intact.
    assert!(table_exists(&pool, "rc_t1").await);
    if let Pool::Sqlite(sq) = &pool {
        let n: i64 = sqlx::query_scalar("SELECT id FROM rc_t1")
            .fetch_one(sq)
            .await
            .unwrap();
        assert_eq!(n, 42, "existing data must survive fake-apply");
    }

    // Idempotent: re-running is a no-op (squash already in the ledger).
    let again = migrate::migrate_pool(&pool, &new).await.unwrap();
    assert!(again.is_empty());
}

/// Cross-ledger reconcile (`--fake-initial`): the replaced names are NOT in
/// *this* ledger (they ran under a different one — e.g. the framework's old
/// hand-built bootstrap vs the new generated system migrations), but the
/// tables already exist. The squash must FAKE-APPLY on table existence alone.
#[tokio::test]
async fn squash_fake_initials_when_tables_already_exist_cross_ledger() {
    let root = fresh_dir("fakeinitial");
    let pool = pool_at(&root).await;

    // Simulate "tables exist, but nothing in this ledger" — as if a different
    // ledger (or hand-built DDL) created them.
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query("CREATE TABLE rc_fi (id INTEGER PRIMARY KEY)")
            .execute(sq)
            .await
            .unwrap();
        sqlx::query("INSERT INTO rc_fi (id) VALUES (7)")
            .execute(sq)
            .await
            .unwrap();
    }

    // The squash replaces names that are NOT in this ledger, and creates a
    // table that already exists.
    let dir = root.join("migrations");
    write(
        &dir,
        &create_table_mig("0001_system", "rc_fi", &["0001_legacy_bootstrap"]),
    );
    let applied = migrate::migrate_pool(&pool, &dir).await.unwrap();

    assert_eq!(
        applied.len(),
        1,
        "recorded (fake-applied) on table existence"
    );
    assert_eq!(ledger_names(&pool).await, vec!["0001_system".to_string()]);
    // No re-create; the pre-existing row survives.
    if let Pool::Sqlite(sq) = &pool {
        let n: i64 = sqlx::query_scalar("SELECT id FROM rc_fi")
            .fetch_one(sq)
            .await
            .unwrap();
        assert_eq!(n, 7);
    }
}

/// DB that applied only PART of the replaced chain: refuse (inconsistent).
#[tokio::test]
async fn squash_on_partially_migrated_db_conflicts() {
    let root = fresh_dir("partial");
    let pool = pool_at(&root).await;

    // Only 0001_a is applied.
    let old = root.join("old");
    write(&old, &create_table_mig("0001_a", "rc_p1", &[]));
    migrate::migrate_pool(&pool, &old).await.unwrap();

    // Squash claims to replace both 0001_a and 0002_b — but 0002_b never ran.
    let new = root.join("squashed");
    write(
        &new,
        &create_table_mig("0001_squash", "rc_p1", &["0001_a", "0002_b"]),
    );
    let err = migrate::migrate_pool(&pool, &new).await.unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("cannot reconcile squash"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("0002_b"),
        "error should name the missing migration: {msg}"
    );
}
