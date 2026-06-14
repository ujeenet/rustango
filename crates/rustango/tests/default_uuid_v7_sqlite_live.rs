#![cfg(feature = "sqlite")]
//! Live SQLite tests for `#[rustango(default_uuid_v7)]` — issue #823.
//!
//! Confirms:
//! * An `Auto<Uuid>` PK with `default_uuid_v7` gets a freshly
//!   generated UUIDv7 on every insert when left as `Auto::Unset`.
//! * The generated PK is **time-sortable**: a row inserted later
//!   compares strictly greater than a row inserted earlier (the v7
//!   spec's `unix_ts_ms` prefix gives this ordering).
//! * A user-supplied `Auto::Set(custom_uuid)` is honored verbatim —
//!   no overwrite.
//! * `save_pool` on an inserted row updates rather than re-inserts.

use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "u7_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<uuid::Uuid>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE u7_post (
            id    TEXT PRIMARY KEY,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn insert_with_unset_pk_generates_uuid_v7() {
    let pool = make_pool().await;
    let mut post = Post {
        id: Auto::default(),
        title: "First".into(),
    };
    post.save_pool(&pool).await.unwrap();
    let id = post.id.get().copied().expect("PK populated");
    // UUIDv7's version nibble is 7.
    assert_eq!(id.get_version_num(), 7, "expected UUIDv7, got {id}");

    // Read it back — it lives in the row.
    let rows: Vec<Post> = rustango::query::QuerySet::<Post>::default()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.get().copied(), Some(id));
}

#[tokio::test]
async fn consecutive_inserts_are_time_sortable() {
    let pool = make_pool().await;
    let mut a = Post {
        id: Auto::default(),
        title: "A".into(),
    };
    a.save_pool(&pool).await.unwrap();
    // A 2ms gap is enough — UUIDv7 carries a unix_ts_ms prefix.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let mut b = Post {
        id: Auto::default(),
        title: "B".into(),
    };
    b.save_pool(&pool).await.unwrap();

    let a_id = a.id.get().copied().unwrap();
    let b_id = b.id.get().copied().unwrap();
    // String comparison of UUIDs corresponds to lexicographic byte
    // order; UUIDv7's leading unix_ts_ms makes lexicographic order
    // equivalent to insertion-time order.
    assert!(
        a_id < b_id,
        "expected A's UUIDv7 < B's UUIDv7 (time-sortable); got A={a_id} B={b_id}",
    );
}

#[tokio::test]
async fn user_supplied_uuid_is_not_overwritten() {
    let pool = make_pool().await;
    let custom = uuid::Uuid::parse_str("01911234-5678-7abc-9def-0123456789ab").unwrap();
    let mut post = Post {
        id: Auto::Set(custom),
        title: "Pre-supplied".into(),
    };
    post.save_pool(&pool).await.unwrap();
    assert_eq!(
        post.id.get().copied(),
        Some(custom),
        "Auto::Set must round-trip unchanged"
    );
}

#[tokio::test]
async fn save_pool_after_insert_does_an_update() {
    let pool = make_pool().await;
    let mut post = Post {
        id: Auto::default(),
        title: "Original".into(),
    };
    post.save_pool(&pool).await.unwrap();
    let pk = post.id.get().copied().unwrap();

    post.title = "Renamed".into();
    post.save_pool(&pool).await.unwrap();

    let rows: Vec<Post> = rustango::query::QuerySet::<Post>::default()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "save_pool should UPDATE not re-INSERT");
    assert_eq!(rows[0].title, "Renamed");
    assert_eq!(rows[0].id.get().copied(), Some(pk));
}
