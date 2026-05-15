#![cfg(feature = "sqlite")]
//! Live SQLite coverage for `__regex` / `__iregex` (issue #26).
//!
//! SQLite's `REGEXP` keyword delegates to a `regexp(pattern, value)`
//! user-function. sqlx-sqlite 0.8 does *not* register one by default
//! (that's behind the `regexp` cargo feature which rustango doesn't
//! enable). We verify two things:
//!
//! 1. **Emission is well-formed** — when `regexp` isn't registered,
//!    SQLite surfaces `no such function: REGEXP` (or `regexp`) at
//!    execution. That's a runtime error from SQLite itself, not a
//!    syntax error from the parser — proving the dialect emits SQL
//!    SQLite accepts up to the function-resolution step.
//!
//! 2. **`__iregex` LOWER-fallback is parser-clean for both sides** —
//!    same shape, SQLite parses `LOWER("col") REGEXP LOWER(?)` and
//!    fails at the same resolution step rather than e.g. mis-quoting
//!    the LOWER call.

use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rx_sqlite_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

async fn seeded_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE rx_sqlite_user (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
        vec![],
    )
    .await
    .unwrap();
    for name in ["alice", "Bob", "ADMIN"] {
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO rx_sqlite_user(name) VALUES (?)",
            vec![rustango::core::SqlValue::String(name.to_owned())],
        )
        .await
        .unwrap();
    }
    let _ = User::SCHEMA; // silence unused-import warning paths
    pool
}

/// `.regex(...)` emits `"name" REGEXP ?` against SQLite. Without a
/// `regexp` user-function registered, SQLite returns
/// `no such function: REGEXP` at execution — proving the SQL is
/// syntactically well-formed and the failure is purely the missing
/// extension.
#[tokio::test]
async fn regex_emission_parses_clean_runtime_error_on_missing_extension() {
    use rustango::sql::FetcherPool;
    let pool = seeded_pool().await;

    let err = User::objects()
        .where_(User::name.regex("^al.*"))
        .fetch_pool(&pool)
        .await
        .expect_err("should error — regexp not registered");

    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("regexp") || msg.contains("function"),
        "expected missing-regexp error, got: {err}"
    );
    // Confirm it's NOT a parse error — the error mentions regexp/function,
    // not `near "REGEXP"` or `syntax error`.
    assert!(
        !msg.contains("syntax error") && !msg.contains("near \"regexp\""),
        "should be runtime not syntax error: {err}"
    );
}

/// Same check for `__iregex` — the LOWER-wrap shape parses cleanly
/// (LOWER is built into SQLite, so the only resolution that can fail
/// is the missing `regexp`).
#[tokio::test]
async fn iregex_lower_wrap_parses_clean_runtime_error() {
    use rustango::sql::FetcherPool;
    let pool = seeded_pool().await;

    let err = User::objects()
        .where_(User::name.iregex("^al.*"))
        .fetch_pool(&pool)
        .await
        .expect_err("should error — regexp not registered");

    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("regexp") || msg.contains("function"),
        "expected missing-regexp error, got: {err}"
    );
    assert!(
        !msg.contains("syntax error"),
        "should be runtime not syntax error: {err}"
    );
}
