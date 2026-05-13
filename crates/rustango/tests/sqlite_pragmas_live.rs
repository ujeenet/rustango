//! v0.40 regression — `Pool::connect("sqlite:…")` must always
//! turn on `foreign_keys`, set `busy_timeout`, and (for file-backed
//! databases) enable WAL journal mode. SQLite ships with FK
//! enforcement OFF, so without this an ORM that emits ForeignKey
//! columns silently accepts orphaned references.

#![cfg(feature = "sqlite")]

use rustango::sql::sqlx::Row;
use rustango::sql::Pool;

async fn pragma_i64(pool: &Pool, pragma: &str) -> i64 {
    let sqlite = pool.as_sqlite().expect("Pool::Sqlite");
    let row = rustango::sql::sqlx::query(&format!("PRAGMA {pragma}"))
        .fetch_one(sqlite)
        .await
        .unwrap_or_else(|e| panic!("PRAGMA {pragma} failed: {e}"));
    row.try_get::<i64, _>(0)
        .unwrap_or_else(|e| panic!("PRAGMA {pragma} returned non-int: {e}"))
}

async fn pragma_str(pool: &Pool, pragma: &str) -> String {
    let sqlite = pool.as_sqlite().expect("Pool::Sqlite");
    let row = rustango::sql::sqlx::query(&format!("PRAGMA {pragma}"))
        .fetch_one(sqlite)
        .await
        .unwrap_or_else(|e| panic!("PRAGMA {pragma} failed: {e}"));
    row.try_get::<String, _>(0)
        .unwrap_or_else(|e| panic!("PRAGMA {pragma} returned non-str: {e}"))
}

#[tokio::test]
async fn in_memory_pool_has_foreign_keys_on_and_busy_timeout_set() {
    let pool = Pool::connect("sqlite::memory:").await.expect("connect");
    assert_eq!(pragma_i64(&pool, "foreign_keys").await, 1);
    assert_eq!(pragma_i64(&pool, "busy_timeout").await, 5000);
}

#[tokio::test]
async fn lazy_pool_has_foreign_keys_on_and_busy_timeout_set() {
    let pool = Pool::connect_lazy("sqlite::memory:").expect("connect_lazy");
    assert_eq!(pragma_i64(&pool, "foreign_keys").await, 1);
    assert_eq!(pragma_i64(&pool, "busy_timeout").await, 5000);
}

#[tokio::test]
async fn file_backed_pool_enables_wal_journal_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("v040.db");
    let url = format!("sqlite:{}", path.display());
    let pool = Pool::connect(&url).await.expect("connect file-backed");
    assert_eq!(pragma_i64(&pool, "foreign_keys").await, 1);
    assert_eq!(pragma_i64(&pool, "busy_timeout").await, 5000);
    assert_eq!(pragma_str(&pool, "journal_mode").await, "wal");
}

#[tokio::test]
async fn foreign_key_violation_is_rejected() {
    // Concrete proof that FK enforcement is wired up: insert a child
    // pointing at a non-existent parent and expect failure. Without
    // `PRAGMA foreign_keys = ON` this would silently succeed.
    let pool = Pool::connect("sqlite::memory:").await.expect("connect");
    let sqlite = pool.as_sqlite().unwrap();
    rustango::sql::sqlx::query("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .execute(sqlite)
        .await
        .unwrap();
    rustango::sql::sqlx::query(
        "CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id))",
    )
    .execute(sqlite)
    .await
    .unwrap();
    let result = rustango::sql::sqlx::query("INSERT INTO child(parent_id) VALUES (999)")
        .execute(sqlite)
        .await;
    assert!(
        result.is_err(),
        "FK violation should be rejected, but the INSERT succeeded"
    );
}
