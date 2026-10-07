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

/// Data operations before a failing DDL are committed by MySQL, so the
/// error must say so rather than claim a clean rollback (#2151).
#[tokio::test]
async fn data_committed_before_a_failing_ddl_is_reported() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let pool = rustango::sql::Pool::Mysql(my.clone());

    let table = unique("pa_data");
    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE TABLE `{table}` (id BIGINT PRIMARY KEY)"))
        .execute(&my)
        .await
        .unwrap();

    // Op 1 inserts a row; op 2 drops a table that does not exist.
    let name = unique("9902_data_then_ddl");
    let mig = rustango::migrate::Migration {
        name: name.clone(),
        created_at: "2026-10-06T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        replaces: vec![],
        snapshot: SchemaSnapshot::default(),
        forward: vec![
            rustango::migrate::Operation::Data(
                serde_json::from_value(serde_json::json!({
                    "sql": format!("INSERT INTO `{table}` (id) VALUES (1)"),
                    "reversible": false
                }))
                .unwrap(),
            ),
            rustango::migrate::Operation::Schema(SchemaChange::DropTable(unique("pa_missing"))),
        ],
    };

    let dir = std::env::temp_dir().join(unique("rustango_data_ddl"));
    std::fs::create_dir_all(&dir).unwrap();
    rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();

    let err = rustango::migrate::migrate_pool(&pool, &dir)
        .await
        .expect_err("the table to drop does not exist");
    let msg = err.to_string();
    assert!(
        matches!(
            err,
            rustango::migrate::MigrateError::PartiallyApplied {
                applied: 1,
                ddl_applied: 0,
                ..
            }
        ),
        "the insert committed, so this is not a clean rollback: {err:?}"
    );
    assert!(
        msg.contains("1 of 2"),
        "the error must say what was applied: {msg}"
    );

    let rows: i64 = sqlx::query(&format!("SELECT COUNT(*) AS c FROM `{table}`"))
        .fetch_one(&my)
        .await
        .unwrap()
        .get::<i64, _>("c");
    assert_eq!(rows, 1, "the insert really committed");

    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
}

fn data_op(sql: String, reverse: Option<String>) -> rustango::migrate::Operation {
    let mut v = serde_json::json!({ "sql": sql, "reversible": reverse.is_some() });
    if let Some(r) = reverse {
        v["reverse_sql"] = r.into();
    }
    rustango::migrate::Operation::Data(serde_json::from_value(v).unwrap())
}

fn migration(
    name: &str,
    forward: Vec<rustango::migrate::Operation>,
) -> rustango::migrate::Migration {
    rustango::migrate::Migration {
        name: name.to_owned(),
        created_at: "2026-10-06T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::default(),
        replaces: vec![],
        snapshot: SchemaSnapshot::default(),
        forward,
    }
}

async fn fresh_table(my: &sqlx::MySqlPool, table: &str) {
    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(my)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE TABLE `{table}` (id BIGINT PRIMARY KEY)"))
        .execute(my)
        .await
        .unwrap();
}

/// A `begin` that fails after the explicit commit still reports the
/// committed data (#2177 review).
#[tokio::test]
async fn a_failed_begin_after_the_commit_is_reported() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let admin = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let table = unique("pa_begin");
    fresh_table(&admin, &table).await;

    // Once the inserted row is committed, the pool hands out no connection,
    // so the `begin` after the explicit commit fails.
    async fn committed(conn: &mut sqlx::MySqlConnection, t: &str) -> bool {
        sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM `{t}`"))
            .fetch_one(conn)
            .await
            .map_or(false, |n| n > 0)
    }
    let (t1, t2) = (table.clone(), table.clone());
    let my = sqlx::mysql::MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(2))
        .before_acquire(move |conn, _| {
            let t = t1.clone();
            Box::pin(async move { Ok(!committed(conn, &t).await) })
        })
        .after_connect(move |conn, _| {
            let t = t2.clone();
            Box::pin(async move {
                if committed(conn, &t).await {
                    Err(sqlx::Error::Protocol("no new connections".into()))
                } else {
                    Ok(())
                }
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let pool = rustango::sql::Pool::Mysql(my);

    let name = unique("9903_begin");
    let mig = migration(
        &name,
        vec![
            data_op(format!("INSERT INTO `{table}` (id) VALUES (1)"), None),
            rustango::migrate::Operation::Schema(SchemaChange::DropTable(unique("pa_missing"))),
        ],
    );
    let dir = std::env::temp_dir().join(unique("rustango_begin"));
    std::fs::create_dir_all(&dir).unwrap();
    rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();

    let err = rustango::migrate::migrate_pool(&pool, &dir)
        .await
        .expect_err("no connection for the second transaction");
    assert!(
        matches!(
            err,
            rustango::migrate::MigrateError::PartiallyApplied { applied: 1, .. }
        ),
        "the insert committed before `begin` failed: {err:?}"
    );

    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&admin)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Atomic unapply on MySQL reports data committed before a failing DDL
/// the same way apply does (#2177 review).
#[tokio::test]
async fn unapply_reports_data_committed_before_a_failing_ddl() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");
    let pool = rustango::sql::Pool::Mysql(my.clone());
    let table = unique("pa_unapply");
    fresh_table(&my, &table).await;

    // Inverted: the INSERT runs, then DROP TABLE of a table that was
    // never created, because the migration is only recorded.
    let name = unique("9904_unapply");
    let mig = migration(
        &name,
        vec![
            rustango::migrate::Operation::Schema(SchemaChange::CreateTable(unique("pa_never"))),
            data_op(
                "SELECT 1".into(),
                Some(format!("INSERT INTO `{table}` (id) VALUES (1)")),
            ),
        ],
    );
    let dir = std::env::temp_dir().join(unique("rustango_unapply"));
    std::fs::create_dir_all(&dir).unwrap();
    rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();
    rustango::migrate::ensure_ledger_pool_with_ledger(&pool, "__rustango_migrations__")
        .await
        .unwrap();
    sqlx::query("INSERT INTO __rustango_migrations__ (name, applied_at) VALUES (?, NOW())")
        .bind(&name)
        .execute(&my)
        .await
        .unwrap();

    let err = rustango::migrate::unapply_force_pool(&pool, &dir, &name)
        .await
        .expect_err("the table to drop does not exist");
    assert!(
        matches!(
            err,
            rustango::migrate::MigrateError::PartiallyApplied {
                applied: 1,
                ddl_applied: 0,
                ..
            }
        ),
        "the insert committed, so this is not a clean rollback: {err:?}"
    );
    let rows: i64 = sqlx::query(&format!("SELECT COUNT(*) AS c FROM `{table}`"))
        .fetch_one(&my)
        .await
        .unwrap()
        .get::<i64, _>("c");
    assert_eq!(rows, 1, "the insert really committed");

    let _ = sqlx::query("DELETE FROM __rustango_migrations__ WHERE name = ?")
        .bind(&name)
        .execute(&my)
        .await;
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
}
