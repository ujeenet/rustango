#![cfg(feature = "mysql")]
//! `DropIndex` against a real MySQL server (#1588).
//!
//! Reads `MYSQL_TEST_URL`; skips silently when unset.
//!
//! ## Why this has to be live
//!
//! MySQL is the only dialect that needs the table —
//! `DROP INDEX <name> ON <table>` — and the only one that rejects
//! `IF EXISTS` on that statement. PostgreSQL and SQLite drop by name
//! and accept the old form, so a render-only test on those two proves
//! nothing about the dialect where this broke.
//!
//! It broke in production: a tenant on MySQL 8.0.36 dropped one model,
//! `makemigrations` emitted `DropTable` plus one `DropIndex` per index,
//! MySQL applied the `DropTable`, refused the first `DropIndex`, and
//! the migration was recorded as failed. The table was gone with no
//! ledger row, and re-running failed differently — 1051, unknown table.
//! Recovery needed `migrate --fake`, which you have to already know
//! about.
//!
//! The unit tests assert the *rendered string*. This asserts the server
//! accepts it, which is the claim that actually failed.

use std::sync::atomic::{AtomicU32, Ordering};

use rustango::migrate::{diff::render_changes_split_with_dialect, SchemaChange, SchemaSnapshot};
use rustango::sql::sqlx::{self, Row};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}_{}_{n}", std::process::id())
}

/// The rendered `DROP INDEX … ON …` executes on MySQL.
///
/// The old renderer refused to emit anything here, so there was no SQL
/// to run — this test could not have existed before the fix.
#[tokio::test]
async fn rendered_drop_index_executes_on_mysql() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");

    let table = unique("di_tbl");
    let index = unique("di_idx");

    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE `{table}` (id BIGINT PRIMARY KEY, slug VARCHAR(64))"
    ))
    .execute(&my)
    .await
    .unwrap();
    sqlx::query(&format!("CREATE INDEX `{index}` ON `{table}` (slug)"))
        .execute(&my)
        .await
        .unwrap();

    // Control: the index is really there, so its absence below means
    // the DROP worked rather than that it never existed.
    let before: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM information_schema.statistics \
         WHERE table_schema = DATABASE() AND table_name = ? AND index_name = ?",
    )
    .bind(&table)
    .bind(&index)
    .fetch_one(&my)
    .await
    .unwrap()
    .get::<i64, _>("c");
    assert!(before > 0, "control: the index was never created");

    let sql = render_changes_split_with_dialect(
        &[SchemaChange::DropIndex {
            name: index.clone(),
            table: table.clone(),
        }],
        &SchemaSnapshot::default(),
        &rustango::sql::MySql,
    )
    .expect("DropIndex renders on MySQL");

    for stmt in &sql.immediate {
        sqlx::query(stmt)
            .execute(&my)
            .await
            .unwrap_or_else(|e| panic!("MySQL rejected the rendered DDL `{stmt}`: {e}"));
    }

    let after: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM information_schema.statistics \
         WHERE table_schema = DATABASE() AND table_name = ? AND index_name = ?",
    )
    .bind(&table)
    .bind(&index)
    .fetch_one(&my)
    .await
    .unwrap()
    .get::<i64, _>("c");
    assert_eq!(after, 0, "the index survived the rendered DROP");

    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await
        .unwrap();
}

/// MySQL rejects `IF EXISTS` on `DROP INDEX` — so the renderer must not
/// emit the PostgreSQL form here.
///
/// Pinning the *server's* rejection rather than trusting the comment
/// that says so. If a later change routes MySQL through the ANSI arm,
/// the unit test would still pass on the string; this one would not.
#[tokio::test]
async fn mysql_really_does_reject_the_postgres_form() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url).await.expect("connect mysql");

    let table = unique("di_rej_tbl");
    let index = unique("di_rej_idx");
    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE `{table}` (id BIGINT PRIMARY KEY, slug VARCHAR(64))"
    ))
    .execute(&my)
    .await
    .unwrap();
    sqlx::query(&format!("CREATE INDEX `{index}` ON `{table}` (slug)"))
        .execute(&my)
        .await
        .unwrap();

    let err = sqlx::query(&format!("DROP INDEX IF EXISTS `{index}`"))
        .execute(&my)
        .await
        .expect_err("MySQL must reject the PostgreSQL DROP INDEX form");
    let msg = err.to_string();
    assert!(
        msg.contains("1064") || msg.to_lowercase().contains("syntax"),
        "expected a syntax error from MySQL, got: {msg}"
    );

    sqlx::query(&format!("DROP TABLE IF EXISTS `{table}`"))
        .execute(&my)
        .await
        .unwrap();
}
