//! Issue #560 — SQLite `bulk_update_pool` runtime failure. Previously
//! `Sqlite::compile_bulk_update` returned
//! `DialectQueryCompilationNotImplemented` so any call to the
//! framework's `bulk_update_pool(&pool, &query)` against a SQLite
//! `Pool` errored at execution time. SQLite's UPDATE-FROM doesn't
//! accept the column-list-alias on an inline `VALUES` the way
//! Postgres does (`AS __data(pk, …)` → `near "(": syntax error`),
//! so the fix routes through a CTE + correlated-subquery writer
//! (`write_bulk_update_sqlite`) that parses on every SQLite with
//! CTE support (3.8.3, 2014).

#![cfg(feature = "sqlite")]

use rustango::core::{BulkUpdateQuery, Model as _, SqlValue};
use rustango::sql::{bulk_update_pool, sqlx, Dialect as _, Pool, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sbu_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 80)]
    pub name: String,
    pub age: i64,
}

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE sbu_user (\
            id INTEGER PRIMARY KEY, \
            name TEXT NOT NULL, \
            age INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&pool)
    .await
    .expect("create table");
    for (i, name) in ["alice", "bob", "carol"].iter().enumerate() {
        sqlx::query("INSERT INTO sbu_user (id, name, age) VALUES (?, ?, ?)")
            .bind((i + 1) as i64)
            .bind(*name)
            .bind(0i64)
            .execute(&pool)
            .await
            .expect("seed");
    }
    Pool::Sqlite(pool)
}

#[test]
fn compile_bulk_update_emits_cte_correlated_subquery_shape() {
    // Previously this errored with DialectQueryCompilationNotImplemented.
    // The PG `FROM (VALUES …) AS __data(...)` shape doesn't parse on
    // SQLite (column-list-alias on inline VALUES → `near "(": syntax
    // error`), so SQLite gets its own CTE + correlated-subquery
    // shape — supported on every SQLite that supports CTEs (3.8.3+).
    let model = <User as rustango::core::Model>::SCHEMA;
    let q = BulkUpdateQuery {
        model,
        update_columns: vec!["name", "age"],
        rows: vec![
            vec![
                SqlValue::I64(1),
                SqlValue::String("Alice".into()),
                SqlValue::I64(30),
            ],
            vec![
                SqlValue::I64(2),
                SqlValue::String("Bob".into()),
                SqlValue::I64(40),
            ],
        ],
    };
    let stmt = Sqlite
        .compile_bulk_update(&q)
        .expect("sqlite bulk_update must compile post-#560");
    assert!(
        stmt.sql
            .starts_with("WITH __data(\"id\", \"name\", \"age\")"),
        "expected CTE prelude; got: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("AS (VALUES (?, ?, ?), (?, ?, ?))"),
        "expected VALUES rows in CTE; got: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("UPDATE \"sbu_user\" SET"),
        "expected UPDATE … SET; got: {}",
        stmt.sql
    );
    assert!(
        stmt.sql
            .contains("WHERE \"id\" IN (SELECT \"id\" FROM __data)"),
        "expected PK IN-subquery; got: {}",
        stmt.sql
    );
    assert_eq!(stmt.params.len(), 6);
}

#[tokio::test]
async fn bulk_update_round_trips_against_sqlite_live_pool() {
    let pool = fresh_pool().await;
    let model = <User as rustango::core::Model>::SCHEMA;
    let q = BulkUpdateQuery {
        model,
        update_columns: vec!["name", "age"],
        rows: vec![
            vec![
                SqlValue::I64(1),
                SqlValue::String("ALICE".into()),
                SqlValue::I64(31),
            ],
            vec![
                SqlValue::I64(3),
                SqlValue::String("CAROL".into()),
                SqlValue::I64(33),
            ],
        ],
    };
    let affected = bulk_update_pool(&pool, &q)
        .await
        .expect("bulk_update_pool must succeed on sqlite post-#560");
    assert_eq!(affected, 2, "two rows in the VALUES set, two updates");

    // Verify the round-trip — alice + carol got updated; bob untouched.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let mut rows: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT id, name, age FROM sbu_user ORDER BY id")
            .fetch_all(sq)
            .await
            .unwrap();
    rows.sort_by_key(|r| r.0);
    assert_eq!(rows[0], (1, "ALICE".into(), 31));
    assert_eq!(rows[1], (2, "bob".into(), 0)); // untouched
    assert_eq!(rows[2], (3, "CAROL".into(), 33));
}
