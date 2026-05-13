#![cfg(feature = "postgres")]
//! Live test for `migrate::Builder` (v0.7 slice 2).
//!
//! Two rustango apps in the same database can pick distinct ledger
//! tables and migrate independently. This file proves it: two
//! Builders, two ledger names, one Postgres database, no collisions.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, Builder, Migration, MigrationScope, Operation, SchemaChange, SchemaSnapshot,
    TableSnapshot,
};
use rustango::sql::sqlx::{self, PgPool, Row};

static COUNTER: AtomicU32 = AtomicU32::new(0);

use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets shared tables (via
/// DROP/CREATE or `drop_all`); under cargo's default parallel harness
/// two tests would race on PG's `pg_type_typname_nsp_index` /
/// `pg_class_relname_nsp_index` system-catalog uniques when both try
/// to CREATE/DROP at once.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        PgPool::connect(&url)
            .await
            .expect("connect to DATABASE_URL"),
    )
}

fn fresh_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_builder_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn unique_suffix() -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{pid}_{n}")
}

fn snapshot_with_table(table_name: &str) -> SchemaSnapshot {
    let table: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": table_name,
        "model": "T",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();
    SchemaSnapshot {
        tables: vec![table],
        ..Default::default()
    }
}

fn make_create_migration(name: &str, table: &str) -> Migration {
    Migration {
        name: name.into(),
        created_at: "2026-04-29T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: MigrationScope::default(),
        snapshot: snapshot_with_table(table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.into()))],
    }
}

fn write_migration(dir: &std::path::Path, mig: &Migration) {
    if !dir.exists() {
        std::fs::create_dir_all(dir).unwrap();
    }
    file::write(&dir.join(format!("{}.json", mig.name)), mig).unwrap();
}

async fn drop_ledger(pool: &PgPool, ledger: &str) {
    let sql = format!(r#"DROP TABLE IF EXISTS "{ledger}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn drop_table(pool: &PgPool, table: &str) {
    let sql = format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

#[tokio::test]
async fn two_builders_keep_distinct_ledgers() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    // Static (`'static`) ledger names — required by `Builder::ledger`.
    // Concrete bytes are arbitrary; just have to be unique within
    // this test file so concurrent runs don't interfere.
    const LEDGER_A: &str = "__rustango_builder_test_a__";
    const LEDGER_B: &str = "__rustango_builder_test_b__";

    drop_ledger(&pool, LEDGER_A).await;
    drop_ledger(&pool, LEDGER_B).await;

    let suffix = unique_suffix();
    let table_a = format!("rustango_builder_table_a_{suffix}");
    let table_b = format!("rustango_builder_table_b_{suffix}");
    let mig_a_name = format!("0001_create_a_{suffix}");
    let mig_b_name = format!("0001_create_b_{suffix}");

    drop_table(&pool, &table_a).await;
    drop_table(&pool, &table_b).await;

    let dir_a = fresh_dir("a");
    let dir_b = fresh_dir("b");
    write_migration(&dir_a, &make_create_migration(&mig_a_name, &table_a));
    write_migration(&dir_b, &make_create_migration(&mig_b_name, &table_b));

    let a = Builder::new().ledger(LEDGER_A);
    let b = Builder::new().ledger(LEDGER_B);
    assert_eq!(a.ledger_name(), LEDGER_A);
    assert_eq!(b.ledger_name(), LEDGER_B);

    let applied_a = a.migrate(&pool, &dir_a).await.unwrap();
    let applied_b = b.migrate(&pool, &dir_b).await.unwrap();
    assert_eq!(applied_a.len(), 1);
    assert_eq!(applied_b.len(), 1);
    assert_eq!(applied_a[0].name, mig_a_name);
    assert_eq!(applied_b[0].name, mig_b_name);

    // Each ledger sees only its own entry.
    let a_set = a.applied_set(&pool).await.unwrap();
    let b_set = b.applied_set(&pool).await.unwrap();
    assert!(a_set.contains(&mig_a_name));
    assert!(!a_set.contains(&mig_b_name));
    assert!(b_set.contains(&mig_b_name));
    assert!(!b_set.contains(&mig_a_name));

    // Both ledger tables exist physically.
    for ledger in [LEDGER_A, LEDGER_B] {
        let exists: bool = sqlx::query(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
        )
        .bind(ledger)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
        assert!(exists, "{ledger} should exist after migrate");
    }

    // Default `migrate::applied_set` (which reads
    // `__rustango_migrations__`) doesn't see entries from custom
    // ledgers. Tolerate noise from other live tests in the same DB —
    // we only assert *absence* of the custom ledger's entries here.
    let default_set = migrate::applied_set(&pool).await.unwrap();
    assert!(!default_set.contains(&mig_a_name));
    assert!(!default_set.contains(&mig_b_name));

    // Cleanup so reruns are clean.
    drop_table(&pool, &table_a).await;
    drop_table(&pool, &table_b).await;
    drop_ledger(&pool, LEDGER_A).await;
    drop_ledger(&pool, LEDGER_B).await;
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[tokio::test]
async fn default_builder_matches_free_function_results() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    Builder::default().ensure_ledger(&pool).await.unwrap();
    let from_builder = Builder::default().applied_set(&pool).await.unwrap();
    let from_free_fn = migrate::applied_set(&pool).await.unwrap();
    assert_eq!(from_builder, from_free_fn);
}

#[tokio::test]
#[should_panic(expected = "is not a valid SQL identifier")]
async fn ledger_name_with_quote_panics_immediately() {
    // No DB call needed — validation runs synchronously at config time.
    let _ = Builder::new().ledger("evil\"name");
}
