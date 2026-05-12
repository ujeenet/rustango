#![cfg(feature = "postgres")]
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
        ..Default::default()
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
        scope: rustango::migrate::MigrationScope::default(),
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
        scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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
        scope: rustango::migrate::MigrationScope::default(),
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
        scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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
            scope: rustango::migrate::MigrationScope::default(),
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

// ---------- regression: FK between sibling CreateTables in one migration ----------

#[tokio::test]
async fn migration_with_two_create_tables_one_having_fk_to_other_applies() {
    // Regression for the bug where the runner emitted FK ALTERs
    // immediately after their CREATE TABLE — failing because the
    // referenced sibling table hadn't been created yet. Fixed by
    // deferring all FK ALTERs to the end of the migration's forward
    // execution.
    let Some(pool) = pool().await else {
        return;
    };
    let suffix = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let parent = format!("fkx_parent_{pid}_{suffix}");
    let child = format!("fkx_child_{pid}_{suffix}");
    let mig_name = format!("0001_fkx_{pid}_{suffix}");
    let dir = fresh_dir("fk_cross");

    // Build a snapshot containing both tables, child with an FK to parent.
    let parent_t: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": parent.clone(),
        "model": "P",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();
    let child_t: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": child.clone(),
        "model": "C",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {
                "name": "parent_id", "column": "parent_id", "ty": "i64",
                "nullable": false, "primary_key": false,
                "fk": {"kind": "fk", "to": parent.clone(), "on": "id"}
            }
        ]
    }))
    .unwrap();
    let snapshot = SchemaSnapshot {
        tables: vec![child_t, parent_t], // child first → exposes the bug
        ..Default::default()
    };

    let mig = Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot,
        forward: vec![
            // Same lex order make_migrations would produce: child < parent.
            Operation::Schema(SchemaChange::CreateTable(child.clone())),
            Operation::Schema(SchemaChange::CreateTable(parent.clone())),
        ],
    };
    write_migration(&dir, &mig);

    drop_table(&pool, &child).await;
    drop_table(&pool, &parent).await;
    delete_ledger_entry(&pool, &mig_name).await;

    migrate::migrate(&pool, &dir).await.unwrap();

    // Both tables exist…
    for t in [&child, &parent] {
        let exists: bool = sqlx::query(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
        )
        .bind(t)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
        assert!(exists, "{t} should exist after migrate");
    }

    // …and the FK is real (Postgres rejects orphan inserts).
    let insert_orphan = sqlx::query(&format!(
        r#"INSERT INTO "{child}" (id, parent_id) VALUES (1, 999)"#
    ))
    .execute(&pool)
    .await;
    assert!(insert_orphan.is_err(), "FK constraint must be live");

    drop_table(&pool, &child).await;
    drop_table(&pool, &parent).await;
    delete_ledger_entry(&pool, &mig_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- migrate_to / downgrade (Slice 5) ----------

/// Build three sequential schema-only migrations creating tables A, B, C.
/// Returns `(dir, mig_names, table_names)` — caller cleans up.
fn three_migrations() -> (PathBuf, [String; 3], [String; 3]) {
    let suffix = unique_table("mt").to_string();
    let dir = fresh_dir("migrate_to");
    let names = [
        format!("0001_{suffix}_a"),
        format!("0002_{suffix}_b"),
        format!("0003_{suffix}_c"),
    ];
    let tables = [
        format!("mt_a_{suffix}"),
        format!("mt_b_{suffix}"),
        format!("mt_c_{suffix}"),
    ];
    let mut prev: Option<String> = None;
    for (i, name) in names.iter().enumerate() {
        write_migration(
            &dir,
            &Migration {
                name: name.clone(),
                created_at: "2026-04-28T00:00:00Z".into(),
                prev: prev.clone(),
                atomic: true,
                scope: rustango::migrate::MigrationScope::default(),
                snapshot: snapshot_with_table(&tables[i]),
                forward: vec![Operation::Schema(SchemaChange::CreateTable(
                    tables[i].clone(),
                ))],
            },
        );
        prev = Some(name.clone());
    }
    (dir, names, tables)
}

async fn cleanup(pool: &PgPool, names: &[String], tables: &[String], dir: &std::path::Path) {
    for n in names {
        delete_ledger_entry(pool, n).await;
    }
    for t in tables {
        drop_table(pool, t).await;
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn migrate_to_unknown_target_is_validation_error() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("migrate_to_unknown");
    let _ = std::fs::create_dir_all(&dir);
    let err = migrate::migrate_to(&pool, &dir, "0042_does_not_exist")
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("0042_does_not_exist"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrate_to_target_already_head_is_noop() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let _ = std::fs::create_dir_all(&dir);
    // Re-write because cleanup removed the dir.
    let (dir, names, tables) = three_migrations();

    migrate::migrate(&pool, &dir).await.unwrap();
    let touched = migrate::migrate_to(&pool, &dir, &names[2]).await.unwrap();
    assert!(touched.is_empty(), "target == head should be a no-op");
    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn migrate_to_forward_applies_pending_subset() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    // Nothing applied yet; migrate_to(0002) should apply 0001 and 0002 only.
    let touched = migrate::migrate_to(&pool, &dir, &names[1]).await.unwrap();
    assert_eq!(touched.len(), 2);
    assert_eq!(touched[0].name, names[0]);
    assert_eq!(touched[1].name, names[1]);

    let applied = migrate::applied_set(&pool).await.unwrap();
    assert!(applied.contains(&names[0]));
    assert!(applied.contains(&names[1]));
    assert!(!applied.contains(&names[2]), "0003 should still be pending");

    // Sanity: tables A and B exist, C doesn't.
    for (i, t) in tables.iter().enumerate() {
        let exists: bool = sqlx::query(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
        )
        .bind(t)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
        if i < 2 {
            assert!(exists, "{t} should exist after migrate_to({})", names[1]);
        } else {
            assert!(!exists, "{t} should NOT exist yet");
        }
    }

    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn migrate_to_backward_unapplies_in_reverse() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    // Apply all three.
    migrate::migrate(&pool, &dir).await.unwrap();

    // Roll back to 0001 — should unapply 0003 then 0002.
    let touched = migrate::migrate_to(&pool, &dir, &names[0]).await.unwrap();
    assert_eq!(touched.len(), 2);
    assert_eq!(touched[0].name, names[2], "0003 first (reverse order)");
    assert_eq!(touched[1].name, names[1], "0002 second");

    let applied = migrate::applied_set(&pool).await.unwrap();
    assert!(applied.contains(&names[0]));
    assert!(!applied.contains(&names[1]));
    assert!(!applied.contains(&names[2]));

    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn migrate_to_zero_unapplies_everything() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    migrate::migrate(&pool, &dir).await.unwrap();
    let touched = migrate::migrate_to(&pool, &dir, "zero").await.unwrap();
    assert_eq!(touched.len(), 3);
    // Reverse-order: C, B, A.
    assert_eq!(touched[0].name, names[2]);
    assert_eq!(touched[1].name, names[1]);
    assert_eq!(touched[2].name, names[0]);

    let applied = migrate::applied_set(&pool).await.unwrap();
    for n in &names {
        assert!(!applied.contains(n), "{n} should be gone");
    }

    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn downgrade_one_step_unapplies_head() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    migrate::migrate(&pool, &dir).await.unwrap();
    let touched = migrate::downgrade(&pool, &dir, 1).await.unwrap();
    assert_eq!(touched.len(), 1);
    assert_eq!(touched[0].name, names[2]);

    let applied = migrate::applied_set(&pool).await.unwrap();
    assert!(applied.contains(&names[0]));
    assert!(applied.contains(&names[1]));
    assert!(!applied.contains(&names[2]));

    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn downgrade_more_steps_than_applied_unapplies_all() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    migrate::migrate(&pool, &dir).await.unwrap();
    let touched = migrate::downgrade(&pool, &dir, 99).await.unwrap();
    assert_eq!(touched.len(), 3);

    let applied = migrate::applied_set(&pool).await.unwrap();
    for n in &names {
        assert!(!applied.contains(n));
    }
    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn downgrade_zero_steps_is_noop() {
    let Some(pool) = pool().await else {
        return;
    };
    let dir = fresh_dir("downgrade_zero");
    let _ = std::fs::create_dir_all(&dir);
    let touched = migrate::downgrade(&pool, &dir, 0).await.unwrap();
    assert!(touched.is_empty());
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

// ---------- v0.4 Slice 3: AlterField + Rename round-trip ----------

#[tokio::test]
async fn alter_column_type_applies_and_unapplies_round_trip() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("alter_type");
    let create_name = unique_migration("alter_type_create", 1);
    let alter_name = unique_migration("alter_type_alter", 2);
    let dir = fresh_dir("alter_type");

    // 0001: create table with `age` as INTEGER (i32).
    let mut create_snap = snapshot_with_table(&table);
    create_snap.tables[0].fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "age", "column": "age", "ty": "i32", "nullable": false, "primary_key": false
        }))
        .unwrap(),
    );
    write_migration(
        &dir,
        &Migration {
            name: create_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: create_snap.clone(),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    // 0002: ALTER age i32 → i64.
    let mut alter_snap = create_snap.clone();
    alter_snap.tables[0]
        .fields
        .iter_mut()
        .find(|f| f.column == "age")
        .unwrap()
        .ty = "i64".into();
    write_migration(
        &dir,
        &Migration {
            name: alter_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: Some(create_name.clone()),
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: alter_snap,
            forward: vec![Operation::Schema(SchemaChange::AlterColumnType {
                table: table.clone(),
                column: "age".into(),
                from: "i32".into(),
                to: "i64".into(),
            })],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &alter_name).await;

    migrate::migrate(&pool, &dir).await.unwrap();

    // After apply: column type is bigint.
    let pg_type: String = sqlx::query(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = 'age'",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert_eq!(pg_type, "bigint", "column should be BIGINT after alter");

    // Unapply 0002 → column reverts to integer.
    migrate::unapply(&pool, &dir, &alter_name).await.unwrap();
    let pg_type: String = sqlx::query(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = 'age'",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert_eq!(pg_type, "integer", "unapply should restore INTEGER");

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &alter_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rename_column_applies_and_unapplies() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("rename_col");
    let create_name = unique_migration("rename_col_create", 1);
    let rename_name = unique_migration("rename_col_rename", 2);
    let dir = fresh_dir("rename_col");

    let mut create_snap = snapshot_with_table(&table);
    create_snap.tables[0].fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "name", "column": "name", "ty": "string", "nullable": false, "primary_key": false, "max_length": 32
        }))
        .unwrap(),
    );
    write_migration(
        &dir,
        &Migration {
            name: create_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: create_snap.clone(),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
        },
    );

    let mut renamed = create_snap.clone();
    renamed.tables[0]
        .fields
        .iter_mut()
        .find(|f| f.column == "name")
        .unwrap()
        .column = "username".into();
    write_migration(
        &dir,
        &Migration {
            name: rename_name.clone(),
            created_at: "2026-04-28T00:00:00Z".into(),
            prev: Some(create_name.clone()),
            atomic: true,
            scope: rustango::migrate::MigrationScope::default(),
            snapshot: renamed,
            forward: vec![Operation::Schema(SchemaChange::RenameColumn {
                table: table.clone(),
                old_column: "name".into(),
                new_column: "username".into(),
            })],
        },
    );

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &rename_name).await;

    migrate::migrate(&pool, &dir).await.unwrap();

    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = 'username')",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists, "renamed column should exist");

    migrate::unapply(&pool, &dir, &rename_name).await.unwrap();
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = 'name')",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(exists, "original column name should be back");

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &create_name).await;
    delete_ledger_entry(&pool, &rename_name).await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- v0.4: migrate_dry_run ----------

#[tokio::test]
async fn dry_run_returns_pending_sql_without_executing() {
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("dry_run");
    let mig_name = unique_migration("dry_run", 1);
    let dir = fresh_dir("dry_run");

    let mig = Migration {
        name: mig_name.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: snapshot_with_table(&table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
    };
    write_migration(&dir, &mig);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    let preview = migrate::migrate_dry_run(&pool, &dir).await.unwrap();
    assert_eq!(preview.len(), 1);
    let p = &preview[0];
    assert_eq!(p.name, mig_name);
    assert!(p.atomic);
    // Sanity: BEGIN, CREATE TABLE..., INSERT INTO __rustango_migrations__, COMMIT.
    assert_eq!(p.statements.first().map(String::as_str), Some("BEGIN"));
    assert_eq!(p.statements.last().map(String::as_str), Some("COMMIT"));
    assert!(
        p.statements.iter().any(|s| s.contains("CREATE TABLE")),
        "{:?}",
        p.statements
    );
    assert!(
        p.statements
            .iter()
            .any(|s| s.contains("__rustango_migrations__")),
        "{:?}",
        p.statements
    );

    // The table must NOT exist — dry-run never writes.
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get(0)
    .unwrap();
    assert!(!exists, "dry-run must never create tables");

    // Ledger must NOT contain the migration.
    let count: i64 = sqlx::query("SELECT COUNT(*) FROM __rustango_migrations__ WHERE name = $1")
        .bind(&mig_name)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(count, 0, "dry-run must never insert into ledger");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn dry_run_skips_already_applied_migrations() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    // Apply 0001 + 0002. Dry-run should preview only 0003.
    migrate::migrate_to(&pool, &dir, &names[1]).await.unwrap();

    let preview = migrate::migrate_dry_run(&pool, &dir).await.unwrap();
    assert_eq!(preview.len(), 1);
    assert_eq!(preview[0].name, names[2]);

    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn dry_run_returns_empty_when_up_to_date() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    migrate::migrate(&pool, &dir).await.unwrap();
    let preview = migrate::migrate_dry_run(&pool, &dir).await.unwrap();
    assert!(preview.is_empty(), "no pending migrations means empty");

    cleanup(&pool, &names, &tables, &dir).await;
}

// ---------- v0.3.1: unapply head check ----------

#[tokio::test]
async fn unapply_refuses_non_head_target() {
    // Apply 0001 → 0002 → 0003. Try to `unapply` 0001 directly.
    // It is not the head, so the call must error before any DB write.
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    migrate::migrate(&pool, &dir).await.unwrap();

    let err = migrate::unapply(&pool, &dir, &names[0]).await.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("out of order"), "got: {msg}");
    assert!(msg.contains(&names[0]), "got: {msg}");
    assert!(msg.contains(&names[2]), "head should be named: {msg}");

    // All three are still in the ledger; nothing was rolled back.
    let applied = migrate::applied_set(&pool).await.unwrap();
    for n in &names {
        assert!(applied.contains(n), "{n} must remain applied");
    }

    cleanup(&pool, &names, &tables, &dir).await;
}

#[tokio::test]
async fn unapply_force_bypasses_head_check() {
    let Some(pool) = pool().await else {
        return;
    };
    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    migrate::migrate(&pool, &dir).await.unwrap();

    // Forcefully drop the middle migration's table even though 0003
    // is still in place. (Schema state is inconsistent afterward —
    // exactly why the default refuses; we only test the bypass here.)
    let target = migrate::unapply_force(&pool, &dir, &names[1])
        .await
        .unwrap();
    assert_eq!(target.name, names[1]);

    let applied = migrate::applied_set(&pool).await.unwrap();
    assert!(applied.contains(&names[0]));
    assert!(!applied.contains(&names[1]), "0002 should be unapplied");
    assert!(applied.contains(&names[2]), "0003 still in ledger");

    cleanup(&pool, &names, &tables, &dir).await;
}

// ---------- v0.3.1: concurrent-migrate advisory lock ----------

#[tokio::test]
async fn concurrent_migrate_calls_serialize_via_advisory_lock() {
    // Without the advisory lock around `migrate`, peers querying
    // `applied_set` simultaneously both see the same pending list, both
    // try to apply, and one loses with either a PK violation on the
    // ledger INSERT or a `relation already exists` from the CREATE.
    // With the lock, peers serialize: across N concurrent calls every
    // migration is applied exactly once and every call returns Ok.
    let Some(pool) = pool().await else {
        return;
    };

    let (dir, names, tables) = three_migrations();
    cleanup(&pool, &names, &tables, &dir).await;
    let (dir, names, tables) = three_migrations();

    let n_tasks = 5_usize;
    let mut handles = Vec::with_capacity(n_tasks);
    for _ in 0..n_tasks {
        let pool = pool.clone();
        let dir = dir.clone();
        handles.push(tokio::spawn(
            async move { migrate::migrate(&pool, &dir).await },
        ));
    }

    let mut total_applied = 0_usize;
    for h in handles {
        let res = h.await.expect("task did not panic");
        let applied = res.expect("each migrate call must succeed under the lock");
        total_applied += applied.len();
    }
    assert_eq!(
        total_applied,
        names.len(),
        "every migration must be applied exactly once across all peers"
    );

    let applied = migrate::applied_set(&pool).await.unwrap();
    for n in &names {
        assert!(applied.contains(n), "{n} must be in the ledger");
    }

    cleanup(&pool, &names, &tables, &dir).await;
}
