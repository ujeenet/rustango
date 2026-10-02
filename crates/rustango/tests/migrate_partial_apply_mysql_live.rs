#![cfg(feature = "mysql")]
//! A migration that fails after committing DDL on MySQL (#1588 part 2).
//!
//! Reads `MYSQL_TEST_URL`; skips silently when unset.
//!
//! ## Why this has to be live
//!
//! The whole defect is MySQL's DDL auto-commit. PostgreSQL rolls a
//! failed migration back completely and SQLite does too, so neither can
//! reach the state under test — the schema moved and the ledger did
//! not. There is nothing here a render-only or single-dialect test
//! could assert.
//!
//! ## The state, from a live tenant
//!
//! `DropTable` succeeded, the next operation failed, and the migration
//! was recorded as failed. So the table was gone with **no ledger
//! row**, and re-running replayed from the top and failed *differently*
//! — `1051 Unknown table`. Nothing reconciled that without
//! `migrate --fake`, which you have to already know exists.
//!
//! These tests assert both halves: that the error names the counts and
//! the recovery path, and that the state it describes is real rather
//! than a comforting fiction. The second test is the control — a
//! failure with no committed DDL must stay a plain driver error, or
//! the advice to `--fake` would be wrong.

use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{SchemaChange, SchemaSnapshot};
use rustango::sql::sqlx::{self, Row};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}_{}_{n}", std::process::id())
}

/// A migration that fails after committing DDL reports the state it
/// left behind, and how to get out of it (#1588 part 2).
///
/// This is the shape from the report: `DropTable` succeeds, the next
/// operation fails, and because MySQL commits DDL immediately the
/// table is gone while the ledger row was never written. Re-running
/// then fails *differently*, so the operator is stuck unless the error
/// tells them about `migrate --fake`.
#[tokio::test]
async fn a_failure_after_committed_ddl_names_the_recovery_path() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let pool = rustango::sql::Pool::Mysql(my.clone());

    let table = unique("pa_tbl");
    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE TABLE `{table}` (id BIGINT PRIMARY KEY)"))
        .execute(&my)
        .await
        .unwrap();

    // Op 1 drops the table (commits, irreversibly). Op 2 is invalid
    // SQL, so the migration dies with op 1 already applied.
    let name = unique("9900_partial");
    let mig = rustango::migrate::Migration {
        name: name.clone(),
        created_at: "2026-09-18T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        replaces: vec![],
        snapshot: SchemaSnapshot::default(),
        forward: vec![
            rustango::migrate::Operation::Schema(SchemaChange::DropTable(table.clone())),
            rustango::migrate::Operation::Data(
                serde_json::from_value(serde_json::json!({
                    "sql": "THIS IS NOT SQL",
                    "reversible": false
                }))
                .unwrap(),
            ),
        ],
    };

    let dir = std::env::temp_dir().join(unique("rustango_partial"));
    std::fs::create_dir_all(&dir).unwrap();
    rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();

    let err = rustango::migrate::migrate_pool(&pool, &dir)
        .await
        .expect_err("the second operation is not valid SQL");

    let msg = err.to_string();
    assert!(
        msg.contains(&name),
        "the error must name the migration that is stuck: {msg}"
    );
    assert!(
        msg.contains("--fake"),
        "the error must name the recovery path — it is otherwise folklore: {msg}"
    );
    assert!(
        msg.contains("1 DDL statement"),
        "the error must say how much DDL committed: {msg}"
    );
    assert!(
        matches!(
            err,
            rustango::migrate::MigrateError::PartiallyApplied { .. }
        ),
        "expected PartiallyApplied so callers can match on it, got: {err:?}"
    );

    // And the state it describes is real: the table is gone and the
    // ledger has no row. Without this the message could be a
    // comforting fiction.
    let tables: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM information_schema.tables \
         WHERE table_schema = DATABASE() AND table_name = ?",
    )
    .bind(&table)
    .fetch_one(&my)
    .await
    .unwrap()
    .get::<i64, _>("c");
    assert_eq!(tables, 0, "the DropTable should have committed");

    let ledger: i64 =
        sqlx::query("SELECT COUNT(*) AS c FROM __rustango_migrations__ WHERE name = ?")
            .bind(&name)
            .fetch_one(&my)
            .await
            .map(|r| r.get::<i64, _>("c"))
            .unwrap_or(0);
    assert_eq!(ledger, 0, "the ledger row must not have been written");

    let _ = std::fs::remove_dir_all(&dir);
}

/// …and a failure with **no** committed DDL is reported unchanged.
///
/// Without this control, `PartiallyApplied` could be returned for every
/// driver error, which would make it meaningless — and would tell
/// operators to `--fake` a migration that rolled back cleanly and
/// should simply be re-run.
#[tokio::test]
async fn a_failure_before_any_ddl_is_still_a_plain_driver_error() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let pool = rustango::sql::Pool::Mysql(my.clone());

    let name = unique("9901_clean");
    let mig = rustango::migrate::Migration {
        name: name.clone(),
        created_at: "2026-09-18T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        replaces: vec![],
        snapshot: SchemaSnapshot::default(),
        // Invalid SQL first — nothing DDL has run when it fails.
        forward: vec![rustango::migrate::Operation::Data(
            serde_json::from_value(serde_json::json!({
                "sql": "THIS IS NOT SQL",
                "reversible": false
            }))
            .unwrap(),
        )],
    };

    let dir = std::env::temp_dir().join(unique("rustango_clean"));
    std::fs::create_dir_all(&dir).unwrap();
    rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();

    let err = rustango::migrate::migrate_pool(&pool, &dir)
        .await
        .expect_err("invalid SQL must fail");
    assert!(
        !matches!(
            err,
            rustango::migrate::MigrateError::PartiallyApplied { .. }
        ),
        "nothing DDL committed, so this rolled back cleanly — telling the operator to \
         `--fake` it would be wrong: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
