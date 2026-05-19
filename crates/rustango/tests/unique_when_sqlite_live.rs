#![cfg(feature = "sqlite")]
//! Live SQLite regression for `#[rustango(unique_when(...))]` —
//! closes #265 / T1.3.
//!
//! Pins that the partial unique constraint:
//!   1. Allows multiple rows where the condition is FALSE (e.g.
//!      soft-deleted rows can all share an email).
//!   2. Rejects a second row where the condition is TRUE (e.g. only
//!      one active email per partition).

use rustango::sql::sqlx;

#[tokio::test]
async fn partial_unique_index_blocks_active_duplicates_on_sqlite() {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");

    // Materialize a schema with the partial unique index. We emit
    // the DDL directly here rather than going through the migration
    // runner; the emission tests (`tests/unique_when_emission.rs`)
    // pin that the migration writer produces this same SQL.
    sqlx::query(
        "CREATE TABLE uw_live_user (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            email      TEXT NOT NULL,
            deleted_at TEXT
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX uw_live_unique_active_email \
         ON uw_live_user (email) WHERE deleted_at IS NULL",
    )
    .execute(&p)
    .await
    .unwrap();

    // First active row: succeeds.
    sqlx::query(r#"INSERT INTO uw_live_user (email, deleted_at) VALUES ('a@x', NULL)"#)
        .execute(&p)
        .await
        .expect("first active insert");
    // Soft-deleted row with the SAME email: succeeds (predicate FALSE).
    sqlx::query(r#"INSERT INTO uw_live_user (email, deleted_at) VALUES ('a@x', '2026-01-01')"#)
        .execute(&p)
        .await
        .expect("soft-deleted shares email");
    // A second soft-deleted row with the same email: also succeeds.
    sqlx::query(r#"INSERT INTO uw_live_user (email, deleted_at) VALUES ('a@x', '2026-02-01')"#)
        .execute(&p)
        .await
        .expect("second soft-delete");

    // Second ACTIVE row with the same email: REJECTED — the partition
    // already has one active row.
    let err = sqlx::query(r#"INSERT INTO uw_live_user (email, deleted_at) VALUES ('a@x', NULL)"#)
        .execute(&p)
        .await
        .expect_err("partial unique must reject second active duplicate");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("unique"),
        "expected unique-constraint violation, got: {msg}"
    );

    // Different active email: succeeds.
    sqlx::query(r#"INSERT INTO uw_live_user (email, deleted_at) VALUES ('b@x', NULL)"#)
        .execute(&p)
        .await
        .expect("different active email");
}
