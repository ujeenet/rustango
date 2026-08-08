#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Fake-initial reconcile for system migrations (#1167 / #1174).
//!
//! When a subsystem that used to build its tables via lazy `ensure_table`
//! DDL becomes managed by a system migration, the first `migrate` after the
//! upgrade must NOT fail on the freshly-generated `CREATE TABLE` colliding
//! with the already-present table. `migrate_pool_with_ledger_fake_initial`
//! records such a pure-`CreateTable`-of-existing-tables migration as applied
//! without running it, leaving existing data intact.
//!
//! SQLite + a temp file (so every pool connection sees the same DB).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};
use rustango::sql::sqlx::{self, Row};
use rustango::sql::Pool;

static COUNTER: AtomicU32 = AtomicU32::new(0);
const LEDGER: &str = "__rustango_system_migrations__";

async fn sqlite_pool() -> (Pool, PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut path = std::env::temp_dir();
    path.push(format!("rustango_fakeinit_{}_{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let sq = sqlx::SqlitePool::connect(&url)
        .await
        .expect("connect sqlite");
    (Pool::Sqlite(sq), path)
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

fn snapshot_with_tables(tables: &[&str]) -> SchemaSnapshot {
    let ts = tables
        .iter()
        .map(|table| {
            serde_json::from_value(serde_json::json!({
                "name": table,
                "model": "T",
                "fields": [
                    {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
                ]
            }))
            .unwrap()
        })
        .collect();
    SchemaSnapshot {
        tables: ts,
        ..Default::default()
    }
}

fn mig(name: &str, snapshot: SchemaSnapshot, forward: Vec<Operation>) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-08-06T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: migrate::MigrationScope::default(),
        replaces: Vec::new(),
        snapshot,
        forward,
    }
}

fn write_dir(mig: &Migration) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut dir = std::env::temp_dir();
    dir.push(format!("rustango_fakeinit_dir_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    file::write(&dir.join(format!("{}.json", mig.name)), mig).unwrap();
    dir
}

async fn ledger_has(pool: &Pool, name: &str) -> bool {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    let count: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) AS c FROM {LEDGER} WHERE name = ?"
    ))
    .bind(name)
    .fetch_one(sq)
    .await
    .unwrap()
    .try_get("c")
    .unwrap();
    count > 0
}

fn cleanup(pool: Pool, path: &Path, dir: &Path) {
    drop(pool);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(dir);
}

/// The reconcile case: a pure-`CreateTable` migration whose table already
/// exists (with data) is faked — recorded in the ledger, its `CREATE TABLE`
/// never run, the existing row untouched.
#[tokio::test]
async fn fake_initial_records_without_running_when_table_exists() {
    let (pool, path) = sqlite_pool().await;
    let table = "recon_media";
    let name = "0004_create_recon_media";
    let m = mig(
        name,
        snapshot_with_table(table),
        vec![Operation::Schema(SchemaChange::CreateTable(table.into()))],
    );
    let dir = write_dir(&m);

    // Simulate the old `ensure_table` era: the table already exists, with data.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query(&format!(
        "CREATE TABLE {table} (id INTEGER PRIMARY KEY, note TEXT)"
    ))
    .execute(sq)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {table} (id, note) VALUES (1, 'pre-existing')"
    ))
    .execute(sq)
    .await
    .unwrap();

    let applied = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER)
        .await
        .expect("fake-initial should succeed, not collide");
    assert_eq!(applied.len(), 1, "migration should be resolved (faked)");
    assert!(
        ledger_has(&pool, name).await,
        "faked migration must be recorded in the ledger"
    );

    // The pre-existing row survived — the CREATE never ran and no data was lost.
    let note: String = sqlx::query(&format!("SELECT note FROM {table} WHERE id = 1"))
        .fetch_one(sq)
        .await
        .unwrap()
        .try_get("note")
        .unwrap();
    assert_eq!(note, "pre-existing");

    // Idempotent: a second run is a no-op (already in ledger).
    let again = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER)
        .await
        .unwrap();
    assert!(again.is_empty(), "second run has nothing pending");

    cleanup(pool, &path, &dir);
}

/// Control: a pure-`CreateTable` migration whose table does NOT exist runs
/// normally (fake-initial doesn't suppress a genuine creation).
#[tokio::test]
async fn fake_initial_creates_normally_when_table_absent() {
    let (pool, path) = sqlite_pool().await;
    let table = "recon_fresh";
    let name = "0001_create_recon_fresh";
    let m = mig(
        name,
        snapshot_with_table(table),
        vec![Operation::Schema(SchemaChange::CreateTable(table.into()))],
    );
    let dir = write_dir(&m);

    let applied = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER)
        .await
        .unwrap();
    assert_eq!(applied.len(), 1);
    assert!(ledger_has(&pool, name).await);

    // The table was actually created (a real INSERT works).
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query(&format!("INSERT INTO {table} (id) VALUES (1)"))
        .execute(sq)
        .await
        .expect("table should have been created by the runner");

    cleanup(pool, &path, &dir);
}

/// **Partial state → create only what's missing.** The `ensure_table` era
/// created framework tables piecemeal — whichever subsystems an app actually
/// touched — so a real upgrade routinely finds *some* of a system migration's
/// tables present and others absent. The runner creates the missing ones and
/// leaves the existing ones (and their data) alone: exactly the
/// `CREATE TABLE IF NOT EXISTS` semantics the retired `ensure_*` calls had.
/// Refusing here instead would simply break the upgrade.
#[tokio::test]
async fn fake_initial_creates_only_missing_tables_on_partial_state() {
    let (pool, path) = sqlite_pool().await;
    let (t1, t2) = ("recon_a", "recon_b");
    let name = "0002_create_recon_a_and_recon_b";
    let m = mig(
        name,
        snapshot_with_tables(&[t1, t2]),
        vec![
            Operation::Schema(SchemaChange::CreateTable(t1.into())),
            Operation::Schema(SchemaChange::CreateTable(t2.into())),
        ],
    );
    let dir = write_dir(&m);

    // Only t1 pre-exists, and it holds data that must survive.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query(&format!(
        "CREATE TABLE {t1} (id INTEGER PRIMARY KEY, note TEXT)"
    ))
    .execute(sq)
    .await
    .unwrap();
    sqlx::query(&format!("INSERT INTO {t1} (id, note) VALUES (1, 'kept')"))
        .execute(sq)
        .await
        .unwrap();

    let applied = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER)
        .await
        .expect("partial state must reconcile, not collide");
    assert_eq!(applied.len(), 1);
    assert!(ledger_has(&pool, name).await);

    // t2 was actually created...
    sqlx::query(&format!("INSERT INTO {t2} (id) VALUES (1)"))
        .execute(sq)
        .await
        .expect("the missing table must have been created");
    // ...and t1's data was left untouched.
    let note: String = sqlx::query(&format!("SELECT note FROM {t1} WHERE id = 1"))
        .fetch_one(sq)
        .await
        .unwrap()
        .try_get("note")
        .unwrap();
    assert_eq!(note, "kept");

    cleanup(pool, &path, &dir);
}

/// **Regression (cross-version proof, #1167):** a *real* generated initial
/// migration is not purely `CreateTable` — `makemigrations` emits the table
/// and then its indexes as sibling ops (the media subsystem's is 4
/// `CreateTable` + 6 `CreateIndex`). An earlier guard demanded literal
/// `CreateTable`-purity, so it bailed on every real migration and
/// fake-initial silently never fired: upgrading a pre-0.51 database still
/// died with `relation "rustango_media" already exists`. This pins the
/// realistic shape.
#[tokio::test]
async fn fake_initial_handles_create_table_plus_indexes() {
    let (pool, path) = sqlite_pool().await;
    let table = "recon_idx";
    let name = "0003_create_recon_idx";
    let m = mig(
        name,
        snapshot_with_table(table),
        vec![
            Operation::Schema(SchemaChange::CreateTable(table.into())),
            Operation::Schema(SchemaChange::CreateIndex {
                name: format!("{table}_id_idx"),
                table: table.into(),
                columns: vec!["id".into()],
                unique: false,
                method: "btree".into(),
                where_clause: None,
                include: Vec::new(),
            }),
        ],
    );
    let dir = write_dir(&m);

    // Pre-existing (ensure_table-era) table with data.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query(&format!(
        "CREATE TABLE {table} (id INTEGER PRIMARY KEY, note TEXT)"
    ))
    .execute(sq)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {table} (id, note) VALUES (1, 'pre-existing')"
    ))
    .execute(sq)
    .await
    .unwrap();

    let applied = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER)
        .await
        .expect("CreateTable+CreateIndex must still reconcile, not collide");
    assert_eq!(applied.len(), 1);
    assert!(ledger_has(&pool, name).await);

    let note: String = sqlx::query(&format!("SELECT note FROM {table} WHERE id = 1"))
        .fetch_one(sq)
        .await
        .unwrap()
        .try_get("note")
        .unwrap();
    assert_eq!(note, "pre-existing");

    cleanup(pool, &path, &dir);
}

/// The index guard has teeth: an index targeting a table this migration does
/// **not** create is real work on a pre-existing table, so the migration must
/// not be faked away.
#[tokio::test]
async fn fake_initial_refuses_index_on_foreign_table() {
    let (pool, path) = sqlite_pool().await;
    let (created, other) = ("recon_new", "recon_other");
    let name = "0004_create_recon_new_and_index_other";
    let m = mig(
        name,
        snapshot_with_table(created),
        vec![
            Operation::Schema(SchemaChange::CreateTable(created.into())),
            // index on a table NOT created here → disqualifies faking
            Operation::Schema(SchemaChange::CreateIndex {
                name: format!("{other}_id_idx"),
                table: other.into(),
                columns: vec!["id".into()],
                unique: false,
                method: "btree".into(),
                where_clause: None,
                include: Vec::new(),
            }),
        ],
    );
    let dir = write_dir(&m);

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    for t in [created, other] {
        sqlx::query(&format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY)"))
            .execute(sq)
            .await
            .unwrap();
    }

    let res = migrate::migrate_pool_with_ledger_fake_initial(&pool, &dir, LEDGER).await;
    assert!(
        res.is_err(),
        "an index on a pre-existing foreign table is real work — must not be faked"
    );

    cleanup(pool, &path, &dir);
}
