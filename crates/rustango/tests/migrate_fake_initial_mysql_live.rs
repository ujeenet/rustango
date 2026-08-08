#![cfg(all(feature = "mysql", feature = "tenancy"))]
//! Fake-initial reconcile on live MySQL (#1167 / #1174).
//!
//! Reads `MYSQL_TEST_URL`; skips silently when unset. Proves the
//! ensure→system-migration upgrade path on the strict dialect: a
//! pre-existing (ensure-era) table is reconciled by recording the
//! generated `CREATE TABLE` migration without running it — no collision,
//! existing data intact.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};
use rustango::sql::sqlx::{self, Row};
use rustango::sql::Pool;

static COUNTER: AtomicU32 = AtomicU32::new(0);
const LEDGER: &str = "__rustango_system_migrations__";

fn table_snap(table: &str) -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": table,
        "model": "T",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap()
}

fn snapshot_with_table(table: &str) -> SchemaSnapshot {
    let t: TableSnapshot = serde_json::from_value(serde_json::json!({
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

fn write_dir(m: &Migration) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut dir = std::env::temp_dir();
    dir.push(format!("rustango_fakeinit_my_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    file::write(&dir.join(format!("{}.json", m.name)), m).unwrap();
    dir
}

#[tokio::test]
async fn fake_initial_reconciles_existing_table_on_mysql() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let pool = Pool::Mysql(my.clone());

    // Unique names so parallel/rerun don't collide on the shared DB + ledger.
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let table = format!("recon_media_{}_{n}", std::process::id());
    let name = format!("9{n:03}_create_{table}");

    // Clean slate.
    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await
        .unwrap();
    let _ = sqlx::query(&format!("DELETE FROM {LEDGER} WHERE name = ?"))
        .bind(&name)
        .execute(&my)
        .await;

    let m = Migration {
        name: name.clone(),
        created_at: "2026-08-06T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: migrate::MigrationScope::default(),
        replaces: Vec::new(),
        snapshot: snapshot_with_table(&table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
    };
    let dir = write_dir(&m);

    // Simulate the old ensure_table era: table already present, with data.
    sqlx::query(&format!(
        "CREATE TABLE `{table}` (id BIGINT PRIMARY KEY, note VARCHAR(64))"
    ))
    .execute(&my)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO `{table}` (id, note) VALUES (1, 'pre-existing')"
    ))
    .execute(&my)
    .await
    .unwrap();

    // The reconcile: no "table already exists" (MySQL 1050); faked instead.
    let applied = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER)
        .await
        .expect("fake-initial must not collide on MySQL");
    assert_eq!(applied.len(), 1);

    // Recorded in the ledger.
    let recorded: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) AS c FROM {LEDGER} WHERE name = ?"
    ))
    .bind(&name)
    .fetch_one(&my)
    .await
    .unwrap()
    .try_get("c")
    .unwrap();
    assert_eq!(recorded, 1, "faked migration must be in the ledger");

    // Data intact — the CREATE never ran.
    let note: String = sqlx::query(&format!("SELECT note FROM `{table}` WHERE id = 1"))
        .fetch_one(&my)
        .await
        .unwrap()
        .try_get("note")
        .unwrap();
    assert_eq!(note, "pre-existing");

    // Cleanup.
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await;
    let _ = sqlx::query(&format!("DELETE FROM {LEDGER} WHERE name = ?"))
        .bind(&name)
        .execute(&my)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    println!("MySQL fake-initial reconcile OK");
}

/// Partial state on live MySQL: some of the migration's tables exist, others
/// don't. The runner must create only the missing ones (MySQL is strict —
/// re-running an existing `CREATE TABLE` is error 1050) and leave the
/// pre-existing table's data alone.
#[tokio::test]
async fn partial_state_creates_only_missing_tables_on_mysql() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let pool = Pool::Mysql(my.clone());

    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let t_have = format!("part_have_{pid}_{n}");
    let t_missing = format!("part_missing_{pid}_{n}");
    let name = format!("8{n:03}_partial_{pid}");

    for t in [&t_have, &t_missing] {
        sqlx::query(&format!("DROP TABLE IF EXISTS `{t}`"))
            .execute(&my)
            .await
            .unwrap();
    }
    let _ = sqlx::query(&format!("DELETE FROM {LEDGER} WHERE name = ?"))
        .bind(&name)
        .execute(&my)
        .await;

    // Only one of the two tables pre-exists, holding data.
    sqlx::query(&format!(
        "CREATE TABLE `{t_have}` (id BIGINT PRIMARY KEY, note VARCHAR(32))"
    ))
    .execute(&my)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO `{t_have}` (id, note) VALUES (1, 'kept')"
    ))
    .execute(&my)
    .await
    .unwrap();

    let m = Migration {
        name: name.clone(),
        created_at: "2026-08-06T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: migrate::MigrationScope::default(),
        replaces: Vec::new(),
        snapshot: SchemaSnapshot {
            tables: vec![table_snap(&t_have), table_snap(&t_missing)],
            ..Default::default()
        },
        forward: vec![
            Operation::Schema(SchemaChange::CreateTable(t_have.clone())),
            Operation::Schema(SchemaChange::CreateTable(t_missing.clone())),
        ],
    };
    let dir = write_dir(&m);

    let applied = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER)
        .await
        .expect("partial state must reconcile on MySQL, not hit error 1050");
    assert_eq!(applied.len(), 1);

    // The missing table really got created.
    sqlx::query(&format!("INSERT INTO `{t_missing}` (id) VALUES (1)"))
        .execute(&my)
        .await
        .expect("missing table should have been created");
    // The pre-existing one kept its data.
    let note: String = sqlx::query(&format!("SELECT note FROM `{t_have}` WHERE id = 1"))
        .fetch_one(&my)
        .await
        .unwrap()
        .try_get("note")
        .unwrap();
    assert_eq!(note, "kept");

    for t in [&t_have, &t_missing] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{t}`"))
            .execute(&my)
            .await;
    }
    let _ = sqlx::query(&format!("DELETE FROM {LEDGER} WHERE name = ?"))
        .bind(&name)
        .execute(&my)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    println!("MySQL partial-state reconcile OK");
}
