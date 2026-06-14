#![cfg(feature = "sqlite")]
//! Live SQLite tests for `QuerySet::active()` / `only_trashed()` /
//! `with_trashed()` — issue #821 (partial: explicit-opt-in shape;
//! auto-scoping a.k.a. global scopes is sibling #820).

use chrono::{DateTime, TimeZone, Utc};
use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sdq_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}

// Model without a soft-delete column — confirms the helpers stay
// no-ops instead of erroring.
#[derive(Model, Debug, Clone)]
#[rustango(table = "sdq_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub label: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE sdq_post (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            title      TEXT NOT NULL,
            deleted_at TEXT
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE sdq_plain (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            label TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    // 3 alive, 2 trashed.
    for (title, deleted) in [
        ("alive-a", None),
        ("alive-b", None),
        ("alive-c", None),
        (
            "trashed-1",
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
        ),
        (
            "trashed-2",
            Some(Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap()),
        ),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            deleted_at: deleted,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn active_filters_to_non_trashed() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Post> = QuerySet::<Post>::default()
        .active()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "expected 3 alive rows: {rows:?}");
    for r in &rows {
        assert!(r.deleted_at.is_none(), "trashed row leaked: {}", r.title);
    }
}

#[tokio::test]
async fn only_trashed_filters_to_trashed() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Post> = QuerySet::<Post>::default()
        .only_trashed()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "expected 2 trashed rows: {rows:?}");
    for r in &rows {
        assert!(r.deleted_at.is_some(), "alive row leaked: {}", r.title);
    }
}

#[tokio::test]
async fn with_trashed_is_no_op_returning_all_rows() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Post> = QuerySet::<Post>::default()
        .with_trashed()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 5, "with_trashed() should not filter anything");
}

#[tokio::test]
async fn active_composes_with_other_filters() {
    let pool = make_pool().await;
    seed(&pool).await;

    // active + title-prefix filter — only "alive-*" rows match
    // (and only by title; "trashed-*" rows would still match the
    // title filter but the active() AND-join excludes them).
    let rows: Vec<Post> = QuerySet::<Post>::default()
        .active()
        .filter("title__startswith", "alive")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn active_is_noop_on_model_without_soft_delete_column() {
    let pool = make_pool().await;
    let mut p1 = Plain {
        id: Auto::default(),
        label: "a".into(),
    };
    p1.save_pool(&pool).await.unwrap();
    let mut p2 = Plain {
        id: Auto::default(),
        label: "b".into(),
    };
    p2.save_pool(&pool).await.unwrap();

    let rows: Vec<Plain> = QuerySet::<Plain>::default()
        .active()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);

    let rows: Vec<Plain> = QuerySet::<Plain>::default()
        .only_trashed()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "only_trashed() no-op on non-SD model");
}
