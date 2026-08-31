//! #1257 — LIKE metacharacter escaping must be honored by the database,
//! on every dialect. Emission is pinned by `filter_lookup.rs`; this file
//! proves the `ESCAPE '!'` clause actually makes `%` / `_` match
//! literally against a real backend.
//!
//! SQLite (embedded) always runs — it was the previously-broken dialect
//! (no default LIKE escape char). Postgres runs when `DATABASE_URL` is
//! set, MySQL when `MYSQL_TEST_URL` is set; both skip silently otherwise.

#![cfg(any(feature = "sqlite", feature = "postgres", feature = "mysql"))]

use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "les_item")]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

const ROWS: &[&str] = &["100%", "1000", "a_b", "axb", "50!off", "plain"];

async fn seed(pool: &Pool) {
    for name in ROWS {
        let mut it = Item {
            id: Auto::default(),
            name: (*name).into(),
        };
        it.save_pool(pool).await.unwrap();
    }
}

async fn names(pool: &Pool, lookup: &str, value: &str) -> Vec<String> {
    let mut rows: Vec<String> = QuerySet::<Item>::default()
        .filter(lookup, value)
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.name)
        .collect();
    rows.sort();
    rows
}

/// The whole point (#1257): `%` and `_` in the search value match
/// literally, not as wildcards — on every dialect the caller ran.
async fn run_matrix(pool: Pool, dialect: &str) {
    seed(&pool).await;

    // `%` is literal: "100%" must match only itself, NOT "1000".
    assert_eq!(
        names(&pool, "name__contains", "100%").await,
        vec!["100%".to_string()],
        "[{dialect}] `%` leaked as a wildcard",
    );

    // `_` is literal: "a_b" must match only itself, NOT "axb".
    assert_eq!(
        names(&pool, "name__contains", "a_b").await,
        vec!["a_b".to_string()],
        "[{dialect}] `_` leaked as a wildcard",
    );

    // The escape char itself (`!`) is literal.
    assert_eq!(
        names(&pool, "name__contains", "50!off").await,
        vec!["50!off".to_string()],
        "[{dialect}] `!` (the escape char) was mishandled",
    );

    // Case-insensitive path (ILIKE / LOWER-LIKE fallback) escapes too.
    assert_eq!(
        names(&pool, "name__icontains", "A_B").await,
        vec!["a_b".to_string()],
        "[{dialect}] icontains did not escape `_`",
    );

    // Sanity: a plain substring still matches as a substring.
    assert_eq!(
        names(&pool, "name__contains", "00").await,
        vec!["100%".to_string(), "1000".to_string()],
        "[{dialect}] literal substring match regressed",
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_honors_like_escape() {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query("CREATE TABLE les_item (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)")
        .execute(&p)
        .await
        .unwrap();
    run_matrix(Pool::Sqlite(p), "sqlite").await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_honors_like_escape() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let p = sqlx::PgPool::connect(&url).await.expect("connect PG");
    sqlx::query(r#"DROP TABLE IF EXISTS "les_item" CASCADE"#)
        .execute(&p)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "les_item" ("id" BIGSERIAL PRIMARY KEY, "name" VARCHAR(80) NOT NULL)"#,
    )
    .execute(&p)
    .await
    .unwrap();
    run_matrix(Pool::Postgres(p), "postgres").await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_honors_like_escape() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        return;
    };
    let p = sqlx::MySqlPool::connect(&url).await.expect("connect MySQL");
    sqlx::query("DROP TABLE IF EXISTS les_item")
        .execute(&p)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE les_item (id BIGINT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(80) NOT NULL)",
    )
    .execute(&p)
    .await
    .unwrap();
    run_matrix(Pool::Mysql(p), "mysql").await;
}
