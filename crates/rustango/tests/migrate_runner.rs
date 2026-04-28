//! Live tests for `rustango::migrate::migrate` (the file-driven runner)
//! and the `__rustango_migrations__` ledger table.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently — same
//! convention as other live tests. Each test uses unique migration
//! names + table names so concurrent runs (within or across binaries)
//! don't collide on the shared ledger or on user tables.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, DataOp, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};
use rustango::sql::sqlx::{self, PgPool, Row};

static COUNTER: AtomicU32 = AtomicU32::new(0);

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
    p.push(format!("rustango_runner_{label}_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn unique_table(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("mr_{prefix}_{pid}_{n}")
}

fn unique_migration(prefix: &str, idx: u32) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{idx:04}_{prefix}_{pid}_{n}")
}

async fn drop_table(pool: &PgPool, table: &str) {
    let sql = format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn delete_ledger_entry(pool: &PgPool, name: &str) {
    sqlx::query("DELETE FROM __rustango_migrations__ WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
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
    }
}

fn write_migration(dir: &std::path::Path, mig: &Migration) {
    if !dir.exists() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let path = dir.join(format!("{}.json", mig.name));
    file::write(&path, mig).unwrap();
}

// ---------- ledger bootstrap ----------

#[tokio::test]
async fn migrate_creates_ledger_table_idempotently() {
    let Some(pool) = pool().await else {
        return;
    };

    // First call ensures table exists.
    let dir = fresh_dir("ledger_bootstrap");
    let _ = migrate::migrate(&pool, &dir).await.unwrap();

    // Schema query: ledger exists.
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_name = '__rustango_migrations__')",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists, "ledger table should exist after migrate");

    // Second call is fine (idempotent CREATE IF NOT EXISTS).
    let _ = migrate::migrate(&pool, &dir).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- empty pending list ----------

#[tokio::test]
async fn migrate_with_empty_dir_returns_empty_list() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("empty_dir");
    let applied = migrate::migrate(&pool, &dir).await.unwrap();
    assert!(applied.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- one migration, schema-only ----------

#[tokio::test]
async fn applies_one_schema_migration_and_records_in_ledger() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("apply_one");
    let mig_name = unique_migration("apply_one", 1);
    let dir = fresh_dir("apply_one");

    let mig = Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        snapshot: snapshot_with_table(&table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
    };
    write_migration(&dir, &mig);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    let applied = migrate::migrate(&pool, &dir).await.unwrap();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].name, mig_name);

    // Table exists.
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists, "{table} should exist after migrate");

    // Ledger has our entry.
    let count: i64 = sqlx::query("SELECT COUNT(*) FROM __rustango_migrations__ WHERE name = $1")
        .bind(&mig_name)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(count, 1);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- re-run is a no-op ----------

#[tokio::test]
async fn rerun_skips_already_applied_migrations() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("rerun");
    let mig_name = unique_migration("rerun", 1);
    let dir = fresh_dir("rerun");

    let mig = Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        snapshot: snapshot_with_table(&table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
    };
    write_migration(&dir, &mig);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    let first = migrate::migrate(&pool, &dir).await.unwrap();
    assert_eq!(first.len(), 1);

    // Second run: nothing pending.
    let second = migrate::migrate(&pool, &dir).await.unwrap();
    assert!(
        second.is_empty(),
        "expected no-op on re-run, got: {second:?}"
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- mid-run failure rolls back the offending file ----------

#[tokio::test]
async fn atomic_failure_rolls_back_offender_keeps_priors() {
    let Some(pool) = pool().await else {
        return;
    };
    let table_a = unique_table("rollback_a");
    let table_b_does_not_exist = unique_table("rollback_b_missing");
    let mig_a = unique_migration("rollback_good", 1);
    let mig_b = unique_migration("rollback_bad", 2);
    let dir = fresh_dir("rollback");

    // Migration A: creates table_a successfully.
    write_migration(
        &dir,
        &Migration {
            name: mig_a.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            snapshot: snapshot_with_table(&table_a),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(
                table_a.clone(),
            ))],
        },
    );

    // Migration B: a data op that references a non-existent table, so it fails.
    write_migration(
        &dir,
        &Migration {
            name: mig_b.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: Some(mig_a.clone()),
            atomic: true,
            // Snapshot includes table_a but the data op references a phantom table.
            snapshot: snapshot_with_table(&table_a),
            forward: vec![Operation::Data(DataOp {
                sql: format!(r#"INSERT INTO "{table_b_does_not_exist}" (id) VALUES (1)"#),
                reverse_sql: None,
                reversible: false,
            })],
        },
    );

    drop_table(&pool, &table_a).await;
    delete_ledger_entry(&pool, &mig_a).await;
    delete_ledger_entry(&pool, &mig_b).await;

    let result = migrate::migrate(&pool, &dir).await;
    assert!(result.is_err(), "B should fail");

    // A's table exists, A is in the ledger.
    let a_exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table_a)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(a_exists, "A's table should be committed (its tx succeeded)");

    let a_count: i64 = sqlx::query("SELECT COUNT(*) FROM __rustango_migrations__ WHERE name = $1")
        .bind(&mig_a)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(a_count, 1, "A should be in the ledger");

    // B is NOT in the ledger.
    let b_count: i64 = sqlx::query("SELECT COUNT(*) FROM __rustango_migrations__ WHERE name = $1")
        .bind(&mig_b)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(b_count, 0, "B's tx rolled back, must not be in ledger");

    drop_table(&pool, &table_a).await;
    delete_ledger_entry(&pool, &mig_a).await;
    delete_ledger_entry(&pool, &mig_b).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- mixed schema + data ops ----------

#[tokio::test]
async fn mixed_schema_and_data_ops_apply_in_order() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("mixed");
    let mig_name = unique_migration("mixed", 1);
    let dir = fresh_dir("mixed");

    // CreateTable then Data op that inserts a row.
    let mig = Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        snapshot: snapshot_with_table(&table),
        forward: vec![
            Operation::Schema(SchemaChange::CreateTable(table.clone())),
            Operation::Data(DataOp {
                sql: format!(r#"INSERT INTO "{table}" (id) VALUES (42)"#),
                reverse_sql: Some(format!(r#"DELETE FROM "{table}" WHERE id = 42"#)),
                reversible: true,
            }),
        ],
    };
    write_migration(&dir, &mig);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    migrate::migrate(&pool, &dir).await.unwrap();

    let id: i64 = sqlx::query(&format!(r#"SELECT id FROM "{table}""#))
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(id, 42);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- atomic = false skips the wrapping tx ----------

#[tokio::test]
async fn non_atomic_migration_runs_outside_a_tx() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("loose");
    let mig_name = unique_migration("loose", 1);
    let dir = fresh_dir("loose");

    let mig = Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: false,
        snapshot: snapshot_with_table(&table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
    };
    write_migration(&dir, &mig);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    let applied = migrate::migrate(&pool, &dir).await.unwrap();
    assert_eq!(applied.len(), 1);

    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- unapply (Slice 4) ----------

#[tokio::test]
async fn unapply_round_trips_a_schema_only_migration() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("unapply_schema");
    let mig_name = unique_migration("unapply_schema", 1);
    let dir = fresh_dir("unapply_schema");

    write_migration(
        &dir,
        &Migration {
            name: mig_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    // Apply.
    migrate::migrate(&pool, &dir).await.unwrap();
    let exists_after_apply: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists_after_apply);

    // Unapply.
    let target = migrate::unapply(&pool, &dir, &mig_name).await.unwrap();
    assert_eq!(target.name, mig_name);

    let exists_after_unapply: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(!exists_after_unapply, "table must be dropped after unapply");

    // Ledger entry is gone.
    let count: i64 = sqlx::query("SELECT COUNT(*) FROM __rustango_migrations__ WHERE name = $1")
        .bind(&mig_name)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(count, 0, "ledger entry must be removed");

    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unapply_data_op_uses_reverse_sql() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("unapply_data");
    let create_name = unique_migration("unapply_create", 1);
    let data_name = unique_migration("unapply_data", 2);
    let dir = fresh_dir("unapply_data");

    // 0001: create table.
    write_migration(
        &dir,
        &Migration {
            name: create_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    // 0002: insert a row, with reverse_sql to delete it.
    write_migration(
        &dir,
        &Migration {
            name: data_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: Some(create_name.clone()),
            atomic: true,
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Data(DataOp {
                sql: format!(r#"INSERT INTO "{table}" (id) VALUES (7)"#),
                reverse_sql: Some(format!(r#"DELETE FROM "{table}" WHERE id = 7"#)),
                reversible: true,
            })],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &data_name).await;

    // Apply both.
    migrate::migrate(&pool, &dir).await.unwrap();
    let count: i64 = sqlx::query(&format!(r#"SELECT COUNT(*) FROM "{table}""#))
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(count, 1, "row should be present after data migration");

    // Unapply only the data migration.
    migrate::unapply(&pool, &dir, &data_name).await.unwrap();

    let count: i64 = sqlx::query(&format!(r#"SELECT COUNT(*) FROM "{table}""#))
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(count, 0, "reverse_sql must have deleted the row");

    // Table still exists (we only rolled back the data migration).
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists, "schema migration was untouched");

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &data_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unapply_irreversible_migration_fails_fast_no_db_writes() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("unapply_irrev");
    let create_name = unique_migration("unapply_irrev_create", 1);
    let bad_name = unique_migration("unapply_irrev_bad", 2);
    let dir = fresh_dir("unapply_irrev");

    // 0001: create table.
    write_migration(
        &dir,
        &Migration {
            name: create_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    // 0002: irreversible data op.
    write_migration(
        &dir,
        &Migration {
            name: bad_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: Some(create_name.clone()),
            atomic: true,
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Data(DataOp {
                sql: format!(r#"INSERT INTO "{table}" (id) VALUES (1)"#),
                reverse_sql: None,
                reversible: false,
            })],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &bad_name).await;

    migrate::migrate(&pool, &dir).await.unwrap();

    // Try to unapply 0002 → should fail fast.
    let err = migrate::unapply(&pool, &dir, &bad_name).await.unwrap_err();
    assert!(format!("{err}").contains("reversible"), "got: {err}");

    // Ledger entry for the bad migration is still present (we never deleted it).
    let count: i64 = sqlx::query("SELECT COUNT(*) FROM __rustango_migrations__ WHERE name = $1")
        .bind(&bad_name)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(
        count, 1,
        "irreversible unapply must NOT have removed the ledger entry"
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &bad_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unapply_unknown_migration_returns_validation_error() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("unapply_unknown");
    let _ = std::fs::create_dir_all(&dir);

    let err = migrate::unapply(&pool, &dir, "0042_does_not_exist")
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("0042_does_not_exist"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unapply_then_reapply_round_trip() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("rt_cycle");
    let mig_name = unique_migration("rt_cycle", 1);
    let dir = fresh_dir("rt_cycle");

    write_migration(
        &dir,
        &Migration {
            name: mig_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            snapshot: snapshot_with_table(&table),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    migrate::migrate(&pool, &dir).await.unwrap();
    migrate::unapply(&pool, &dir, &mig_name).await.unwrap();
    // Apply again — should be idempotent (table re-created).
    let applied = migrate::migrate(&pool, &dir).await.unwrap();
    assert_eq!(
        applied.len(),
        1,
        "after unapply, migration is pending again"
    );

    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- applied_set helper ----------

#[tokio::test]
async fn applied_set_returns_recorded_names() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::ensure_ledger(&pool).await.unwrap();

    let unique = format!("rustango_test_applied_set_{}", std::process::id());
    sqlx::query("INSERT INTO __rustango_migrations__ (name) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(&unique)
        .execute(&pool)
        .await
        .unwrap();

    let set = migrate::applied_set(&pool).await.unwrap();
    assert!(set.contains(&unique), "applied_set should include {unique}");

    delete_ledger_entry(&pool, &unique).await;
}
