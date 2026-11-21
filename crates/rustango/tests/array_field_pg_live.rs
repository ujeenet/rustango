#![cfg(feature = "postgres")]
//! Live PostgreSQL round-trip for `Array<T>` columns
//! (#341). Proves the typed field wrapper writes a native
//! PG array (`text[]` / `integer[]`) on INSERT and decodes it back into
//! `Array<T>` on SELECT, and that the `@>` containment operator filters
//! on it.
//!
//! Skips silently when `DATABASE_URL` is unset (runs in CI's
//! `postgres_test` job).

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Array, Auto, FetcherPool as _, Pool};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "arr_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub tags: Array<String>,
    pub scores: Array<i32>,
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}"));
    Some(pg.into())
}

async fn fresh(pool: &Pool) {
    let pg = pool.as_postgres().expect("postgres pool");
    sqlx::query(r#"DROP TABLE IF EXISTS "arr_post" CASCADE"#)
        .execute(pg)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "arr_post" (
            "id"     BIGSERIAL PRIMARY KEY,
            "title"  VARCHAR(200) NOT NULL,
            "tags"   text[] NOT NULL DEFAULT '{}',
            "scores" integer[] NOT NULL DEFAULT '{}'
        )"#,
    )
    .execute(pg)
    .await
    .unwrap();
}

async fn insert(pool: &Pool, title: &str, tags: &[&str], scores: &[i32]) -> i64 {
    let mut p = Post {
        id: Auto::default(),
        title: title.to_owned(),
        tags: Array(tags.iter().map(|s| (*s).to_owned()).collect()),
        scores: Array(scores.to_vec()),
    };
    p.save(pool).await.unwrap();
    *p.id.get().unwrap()
}

#[tokio::test]
async fn array_columns_round_trip() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let id = insert(&pool, "hello", &["rust", "orm", "pg"], &[10, 20, 30]).await;

    let row = Post::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .expect("row present");
    assert_eq!(row.title, "hello");
    assert_eq!(&*row.tags, &["rust", "orm", "pg"]);
    assert_eq!(&*row.scores, &[10, 20, 30]);
}

#[tokio::test]
async fn empty_array_round_trips() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let id = insert(&pool, "empty", &[], &[]).await;
    let row = Post::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .unwrap();
    assert!(row.tags.is_empty());
    assert!(row.scores.is_empty());
}

/// A mixed array is refused before the bind, which kept only the first
/// element's type; NULL elements are bound, not dropped.
#[tokio::test]
async fn a_mixed_array_is_refused_and_nulls_are_kept() {
    use rustango::core::SqlValue;
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;
    let id = insert(&pool, "mixed", &[], &[5]).await;
    let set = r#"UPDATE "arr_post" SET "scores" = $1 WHERE "id" = $2"#;
    let mixed = SqlValue::Array(vec![SqlValue::I32(1), SqlValue::String("2".into())]);
    let err = rustango::sql::raw_execute_pool(&pool, set, vec![mixed, SqlValue::I64(id)])
        .await
        .expect_err("a mixed array bound");
    assert!(err.to_string().contains("mixed array"), "{err}");
    // No element names the type; PG would get an int4[] for any column.
    let all_null = SqlValue::Array(vec![SqlValue::Null, SqlValue::Null]);
    let err = rustango::sql::raw_execute_pool(&pool, set, vec![all_null, SqlValue::I64(id)])
        .await
        .expect_err("an all-NULL array bound");
    assert!(err.to_string().contains("all-NULL array"), "{err}");
    let with_null = SqlValue::Array(vec![SqlValue::I32(1), SqlValue::Null, SqlValue::I32(2)]);
    rustango::sql::raw_execute_pool(&pool, set, vec![with_null, SqlValue::I64(id)])
        .await
        .expect("an array with a NULL");
    let (scores,): (String,) =
        sqlx::query_as(r#"SELECT "scores"::text FROM "arr_post" WHERE "id" = $1"#)
            .bind(id)
            .fetch_one(pool.as_postgres().unwrap())
            .await
            .unwrap();
    assert_eq!(scores, "{1,NULL,2}");
}

#[tokio::test]
async fn array_contains_operator_filters() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    insert(&pool, "rusty", &["rust", "orm"], &[1]).await;
    insert(&pool, "pythonic", &["python", "orm"], &[2]).await;

    // `tags @> ARRAY['rust']` — only the first post.
    let mut titles: Vec<String> = Post::objects()
        .where_(Post::tags.array_contains(["rust".to_owned()]))
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.title)
        .collect();
    titles.sort();
    assert_eq!(titles, vec!["rusty"]);

    // `tags @> ARRAY['orm']` — both posts.
    let n = Post::objects()
        .where_(Post::tags.array_contains(["orm".to_owned()]))
        .fetch(&pool)
        .await
        .unwrap()
        .len();
    assert_eq!(n, 2);
}
