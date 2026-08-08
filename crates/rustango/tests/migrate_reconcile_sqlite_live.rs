#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Squash reconciliation (#1167) — `Migration.replaces`.
//!
//! A squash collapses historical migrations into one file that recreates the
//! same end state. The runner must therefore behave differently depending on
//! what the database already contains:
//!
//! | database state                              | expected                    |
//! |---------------------------------------------|-----------------------------|
//! | fresh (no history, no tables)               | run the squash for real     |
//! | replaced migrations in the ledger           | fake + tombstone them       |
//! | tables exist but ledger has no history      | fake (cross-ledger)         |
//! | only *some* replaced rows / tables present  | refuse (partial state)      |
//!
//! SQLite on a temp file, so every pool connection sees the same database.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};
use rustango::sql::sqlx::{self, Row};
use rustango::sql::Pool;

static COUNTER: AtomicU32 = AtomicU32::new(0);
const LEDGER: &str = "__rustango_migrations__";

async fn sqlite_pool() -> (Pool, PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut path = std::env::temp_dir();
    path.push(format!("rustango_reconcile_{}_{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let sq = sqlx::SqlitePool::connect(&url)
        .await
        .expect("connect sqlite");
    (Pool::Sqlite(sq), path)
}

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

fn snapshot_of(tables: &[&str]) -> SchemaSnapshot {
    SchemaSnapshot {
        tables: tables.iter().map(|t| table_snap(t)).collect(),
        ..Default::default()
    }
}

/// A migration creating `tables`, optionally declaring a `replaces` list.
fn mig(name: &str, tables: &[&str], replaces: &[&str]) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-08-06T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: migrate::MigrationScope::default(),
        replaces: replaces.iter().map(|s| (*s).to_owned()).collect(),
        snapshot: snapshot_of(tables),
        forward: tables
            .iter()
            .map(|t| Operation::Schema(SchemaChange::CreateTable((*t).to_owned())))
            .collect(),
    }
}

fn fresh_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut dir = std::env::temp_dir();
    dir.push(format!("rustango_reconcile_dir_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, m: &Migration) {
    file::write(&dir.join(format!("{}.json", m.name)), m).unwrap();
}

async fn ledger_names(pool: &Pool) -> Vec<String> {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query(&format!("SELECT name FROM {LEDGER} ORDER BY name"))
        .fetch_all(sq)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.try_get::<String, _>("name").unwrap())
        .collect()
}

async fn table_exists(pool: &Pool, table: &str) -> bool {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?")
        .bind(table)
        .fetch_optional(sq)
        .await
        .unwrap()
        .is_some()
}

fn cleanup(pool: Pool, path: &Path, dir: &Path) {
    drop(pool);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(dir);
}

/// **Fresh database** — nothing applied, no tables. The squash is the only
/// file present, so it must actually RUN (create the tables), not fake.
#[tokio::test]
async fn squash_runs_for_real_on_a_fresh_database() {
    let (pool, path) = sqlite_pool().await;
    let dir = fresh_dir();
    let squash = mig("0001_squashed", &["sq_a", "sq_b"], &["0001_a", "0002_b"]);
    write(&dir, &squash);

    let applied = migrate::migrate_pool(&pool, &dir).await.unwrap();
    assert_eq!(applied.len(), 1);
    assert!(table_exists(&pool, "sq_a").await, "squash must create sq_a");
    assert!(table_exists(&pool, "sq_b").await, "squash must create sq_b");
    assert_eq!(ledger_names(&pool).await, vec!["0001_squashed"]);

    cleanup(pool, &path, &dir);
}

/// **Same-ledger reconcile** — the replaced migrations already ran, so their
/// tables exist and their rows are in the ledger. The squash must be recorded
/// and its predecessors tombstoned, with no DDL (no collision).
#[tokio::test]
async fn squash_fakes_and_tombstones_when_predecessors_applied() {
    let (pool, path) = sqlite_pool().await;
    let dir = fresh_dir();

    // Historical chain runs first.
    write(&dir, &mig("0001_a", &["sq2_a"], &[]));
    write(&dir, &mig("0002_b", &["sq2_b"], &[]));
    let first = migrate::migrate_pool(&pool, &dir).await.unwrap();
    assert_eq!(first.len(), 2);
    // Prove the history is real data we must not lose.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query("INSERT INTO sq2_a (id) VALUES (7)")
        .execute(sq)
        .await
        .unwrap();

    // Now the squash lands, collapsing both.
    let squash = mig("0003_squashed", &["sq2_a", "sq2_b"], &["0001_a", "0002_b"]);
    write(&dir, &squash);
    let applied = migrate::migrate_pool(&pool, &dir)
        .await
        .expect("squash must reconcile, not collide");
    assert_eq!(applied.len(), 1);

    // Squash recorded; both predecessors tombstoned.
    assert_eq!(ledger_names(&pool).await, vec!["0003_squashed"]);

    // Data survived — no DDL ran.
    let n: i64 = sqlx::query("SELECT id FROM sq2_a")
        .fetch_one(sq)
        .await
        .unwrap()
        .try_get("id")
        .unwrap();
    assert_eq!(n, 7);

    // Idempotent: nothing left pending.
    assert!(migrate::migrate_pool(&pool, &dir).await.unwrap().is_empty());

    cleanup(pool, &path, &dir);
}

/// **Cross-ledger reconcile** (Django's `--fake-initial`) — the tables exist
/// but this ledger has no record of the replaced migrations (e.g. history
/// tracked elsewhere, or tables built out-of-band). The squash must fake
/// rather than collide.
#[tokio::test]
async fn squash_fakes_when_tables_exist_but_ledger_is_empty() {
    let (pool, path) = sqlite_pool().await;
    let dir = fresh_dir();

    // Tables exist with data, but nothing is recorded in the ledger.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    for t in ["sq3_a", "sq3_b"] {
        sqlx::query(&format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY)"))
            .execute(sq)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO sq3_a (id) VALUES (42)")
        .execute(sq)
        .await
        .unwrap();

    let squash = mig("0003_squashed", &["sq3_a", "sq3_b"], &["0001_a", "0002_b"]);
    write(&dir, &squash);

    let applied = migrate::migrate_pool(&pool, &dir)
        .await
        .expect("cross-ledger fake-initial must not collide");
    assert_eq!(applied.len(), 1);
    assert_eq!(ledger_names(&pool).await, vec!["0003_squashed"]);

    let n: i64 = sqlx::query("SELECT id FROM sq3_a")
        .fetch_one(sq)
        .await
        .unwrap()
        .try_get("id")
        .unwrap();
    assert_eq!(n, 42, "existing data must be untouched");

    cleanup(pool, &path, &dir);
}

/// **Partial ledger state** — only one of the two replaced migrations is
/// recorded. No automatic choice is safe, so the runner must refuse and say
/// which one is missing.
#[tokio::test]
async fn squash_refuses_partial_ledger_state() {
    let (pool, path) = sqlite_pool().await;
    let dir = fresh_dir();

    // Only 0001_a runs.
    write(&dir, &mig("0001_a", &["sq4_a"], &[]));
    migrate::migrate_pool(&pool, &dir).await.unwrap();

    // Squash claims to replace both 0001_a (applied) and 0002_b (never ran).
    let squash = mig("0003_squashed", &["sq4_a", "sq4_b"], &["0001_a", "0002_b"]);
    write(&dir, &squash);

    let err = migrate::migrate_pool(&pool, &dir)
        .await
        .expect_err("a partial state must be refused, not guessed");
    let msg = format!("{err}");
    assert!(msg.contains("partial state"), "unexpected message: {msg}");
    assert!(msg.contains("0002_b"), "should name the missing one: {msg}");

    // Nothing was recorded or tombstoned.
    assert_eq!(ledger_names(&pool).await, vec!["0001_a"]);

    cleanup(pool, &path, &dir);
}

/// **Partial table state** on the cross-ledger path — the ledger knows
/// nothing, and only one of the squash's tables exists. Also refused.
#[tokio::test]
async fn squash_refuses_partial_table_state() {
    let (pool, path) = sqlite_pool().await;
    let dir = fresh_dir();

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query("CREATE TABLE sq5_a (id INTEGER PRIMARY KEY)")
        .execute(sq)
        .await
        .unwrap();

    let squash = mig("0003_squashed", &["sq5_a", "sq5_b"], &["0001_a", "0002_b"]);
    write(&dir, &squash);

    let err = migrate::migrate_pool(&pool, &dir)
        .await
        .expect_err("partial table state must be refused");
    let msg = format!("{err}");
    assert!(msg.contains("partial state"), "unexpected message: {msg}");
    assert!(ledger_names(&pool).await.is_empty());

    cleanup(pool, &path, &dir);
}

/// A plain (non-squash) migration must NOT be faked just because its table
/// happens to exist — only squashes and the framework's system path do that.
/// Guards against the reconcile logic leaking into ordinary user migrations.
#[tokio::test]
async fn plain_migration_is_not_faked_when_table_exists() {
    let (pool, path) = sqlite_pool().await;
    let dir = fresh_dir();

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query("CREATE TABLE sq6_a (id INTEGER PRIMARY KEY)")
        .execute(sq)
        .await
        .unwrap();

    write(&dir, &mig("0001_a", &["sq6_a"], &[])); // no `replaces`
    let res = migrate::migrate_pool(&pool, &dir).await;
    assert!(
        res.is_err(),
        "a plain migration must still collide loudly, not silently fake"
    );

    cleanup(pool, &path, &dir);
}
