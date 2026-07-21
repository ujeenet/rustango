#![cfg(feature = "postgres")]
//! Tests for `migrate::migrate_embedded` and the `embed_migrations!`
//! proc-macro.

use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{
    self, file, MigrateError, Migration, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
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

fn unique_table(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("emb_{prefix}_{pid}_{n}")
}

fn unique_migration(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("0001_{prefix}_{pid}_{n}")
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

fn make_migration_json(table: &str, name: &str) -> String {
    let mig = Migration {
        name: name.to_owned(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: snapshot_with_table(table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(
            table.to_owned(),
        ))],
    };
    serde_json::to_string(&mig).unwrap()
}

// ---------------- migrate_embedded ----------------

#[tokio::test]
async fn migrate_embedded_applies_pending_then_is_noop_on_rerun() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("apply");
    let mig_name = unique_migration("apply");
    let json = make_migration_json(&table, &mig_name);

    drop_table(&pool, &table).await;
    delete_ledger_entry(&pool, &mig_name).await;

    let embedded: &[(&str, &str)] = &[(&mig_name, &json)];
    let applied = migrate::migrate_embedded(&pool, embedded).await.unwrap();
    assert_eq!(applied.len(), 1);

    // Re-run is a no-op (same logic as `migrate`).
    let applied2 = migrate::migrate_embedded(&pool, embedded).await.unwrap();
    assert!(applied2.is_empty());

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
}

#[tokio::test]
async fn migrate_embedded_rejects_key_name_mismatch() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let table = unique_table("mismatch");
    let real_name = unique_migration("mismatch");
    let json = make_migration_json(&table, &real_name);

    let embedded: &[(&str, &str)] = &[("0042_wrong_key", &json)];
    let err = migrate::migrate_embedded(&pool, embedded)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("0042_wrong_key"), "{msg}");
    assert!(msg.contains(&real_name), "{msg}");
}

#[tokio::test]
async fn migrate_embedded_propagates_parse_errors() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let embedded: &[(&str, &str)] = &[("0001_busted", "{not json")];
    let err = migrate::migrate_embedded(&pool, embedded)
        .await
        .unwrap_err();
    matches!(err, MigrateError::Json(_));
}

#[tokio::test]
async fn migrate_embedded_validates_inconsistent_data_op() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    // reversible=true with no reverse_sql is a contradiction — `parse`
    // should reject it.
    let raw = serde_json::json!({
        "name": "0001_bad_data",
        "created_at": "2026-04-28T00:00:00Z",
        "snapshot": {"tables": []},
        "forward": [{"data": {"sql": "UPDATE x SET y = 1", "reversible": true}}]
    })
    .to_string();
    let embedded: &[(&str, &str)] = &[("0001_bad_data", &raw)];
    let err = migrate::migrate_embedded(&pool, embedded)
        .await
        .unwrap_err();
    matches!(err, MigrateError::Validation(_));
}

#[tokio::test]
async fn migrate_embedded_sorts_entries_lexicographically() {
    // Even when the slice is in the "wrong" order, migrate_embedded
    // applies them in lex order.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let suffix = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let parent = format!("emb_sort_parent_{pid}_{suffix}");
    let child = format!("emb_sort_child_{pid}_{suffix}");
    let mig_a = format!("0001_{pid}_{suffix}_a");
    let mig_b = format!("0002_{pid}_{suffix}_b");
    let json_a = make_migration_json(&parent, &mig_a);
    let json_b = make_migration_json(&child, &mig_b);

    drop_table(&pool, &child).await;
    drop_table(&pool, &parent).await;
    delete_ledger_entry(&pool, &mig_a).await;
    delete_ledger_entry(&pool, &mig_b).await;

    // Slice in B-then-A order; migrate_embedded must still apply A first.
    let embedded: &[(&str, &str)] = &[(&mig_b, &json_b), (&mig_a, &json_a)];
    let applied = migrate::migrate_embedded(&pool, embedded).await.unwrap();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0].name, mig_a);
    assert_eq!(applied[1].name, mig_b);

    drop_table(&pool, &child).await;
    drop_table(&pool, &parent).await;
    delete_ledger_entry(&pool, &mig_a).await;
    delete_ledger_entry(&pool, &mig_b).await;
}

#[tokio::test]
async fn migrate_embedded_rejects_broken_prev_chain() {
    // Same chain validation as `file::list_dir`: an embedded entry
    // declaring `prev` against a sibling that isn't in the slice
    // fails fast with a clear error, before any DB work.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let suffix = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let table = format!("emb_chain_{pid}_{suffix}");
    let orphan = format!("0002_orphan_{pid}_{suffix}");

    let mig = Migration {
        name: orphan.clone(),
        created_at: "2026-04-28T00:00:00Z".into(),
        prev: Some("0001_missing_predecessor".into()),
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        snapshot: snapshot_with_table(&table),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.clone()))],
    };
    let json = serde_json::to_string(&mig).unwrap();

    let embedded: &[(&str, &str)] = &[(&orphan, &json)];
    let err = migrate::migrate_embedded(&pool, embedded)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("broken migration chain"), "got: {msg}");
    assert!(msg.contains(&orphan), "got: {msg}");
    assert!(msg.contains("0001_missing_predecessor"), "got: {msg}");
}

#[tokio::test]
async fn migrate_embedded_empty_slice_is_safe_noop() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let applied = migrate::migrate_embedded(&pool, &[]).await.unwrap();
    assert!(applied.is_empty());
}

// ---------------- embed_migrations! macro ----------------
//
// Compile-time check: invoke the macro on a real fixture directory
// and verify the slice it produces matches what the files contain.

const EMBEDDED_FIXTURE: &[(&str, &str)] = rustango::embed_migrations!("./tests/migrations_fixture");

#[test]
fn embed_migrations_macro_produces_slice_in_lex_order() {
    assert_eq!(
        EMBEDDED_FIXTURE.len(),
        2,
        "expected 2 fixtures, got {}",
        EMBEDDED_FIXTURE.len()
    );
    assert_eq!(EMBEDDED_FIXTURE[0].0, "0001_initial");
    assert_eq!(EMBEDDED_FIXTURE[1].0, "0002_marker");
}

#[test]
fn embed_migrations_entries_round_trip_through_parse() {
    for (name, json) in EMBEDDED_FIXTURE {
        let mig = file::parse(json).unwrap();
        assert_eq!(&mig.name, name, "key/name mismatch in fixture");
    }
}

#[test]
fn embed_migrations_chain_is_validated_at_compile_time() {
    // The fact that this binary compiles is itself the test: the
    // `embed_migrations!("./tests/migrations_fixture")` invocation at
    // the top of this file walks the chain at macro-expansion time
    // (slice 5: v0.4) and would emit a `compile_error!` for a broken
    // `prev` reference, a missing/orphaned predecessor, or a file
    // stem that disagreed with the embedded `name`. The fixture's
    // `0002_marker` declares prev="0001_initial" which exists, so
    // expansion succeeds. Manually flip the `prev` field to a
    // non-existent name to verify: `cargo build` fails with the
    // chain-validation message.
    assert_eq!(EMBEDDED_FIXTURE.len(), 2);
    let prev_field = EMBEDDED_FIXTURE
        .iter()
        .map(|(_name, json)| file::parse(json).expect("fixture parses").prev)
        .collect::<Vec<_>>();
    assert_eq!(prev_field[0], None, "0001 has no prev");
    assert_eq!(
        prev_field[1].as_deref(),
        Some("0001_initial"),
        "0002 chains to 0001"
    );
}

#[test]
#[allow(clippy::const_is_empty)] // Whole point of this test is verifying the const came out empty.
fn embed_migrations_default_path_compiles() {
    // Just verifies that calling embed_migrations!() with no argument
    // doesn't blow up at compile time. The default path is
    // "./migrations" which doesn't exist in this crate, so the slice
    // will be empty — but that's fine; we're testing macro plumbing.
    const E: &[(&str, &str)] = rustango::embed_migrations!();
    assert!(
        E.is_empty(),
        "expected empty slice for missing dir, got {} entries",
        E.len()
    );
}
