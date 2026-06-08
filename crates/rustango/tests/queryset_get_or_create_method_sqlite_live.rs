#![cfg(feature = "sqlite")]
//! Live SQLite test for chainable `QuerySet::get_or_create` /
//! `QuerySet::first_or_create` / `QuerySet::update_or_create`
//! methods — sugar over the existing `rustango::sql::get_or_create`
//! and `update_or_create` free functions.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "goc_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub slug: String,
    #[rustango(max_length = 80)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE goc_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            slug  TEXT NOT NULL,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

#[tokio::test]
async fn get_or_create_creates_when_missing() {
    let pool = make_pool().await;
    let (post, created) = Post::objects()
        .filter("slug", "hello".to_string())
        .get_or_create(
            |pool| async move {
                let mut p = Post {
                    id: Auto::default(),
                    slug: "hello".into(),
                    title: "Hello!".into(),
                };
                p.save_pool(&pool).await?;
                Ok(p)
            },
            &pool,
        )
        .await
        .unwrap();
    assert!(created);
    assert_eq!(post.title, "Hello!");
}

#[tokio::test]
async fn get_or_create_returns_existing_row() {
    let pool = make_pool().await;
    let mut existing = Post {
        id: Auto::default(),
        slug: "hello".into(),
        title: "Original".into(),
    };
    existing.save_pool(&pool).await.unwrap();

    let (post, created) = Post::objects()
        .filter("slug", "hello".to_string())
        .get_or_create(
            |_pool| async move {
                panic!("should not reach create_fn — row already exists");
            },
            &pool,
        )
        .await
        .unwrap();
    assert!(!created);
    assert_eq!(post.title, "Original");
}

#[tokio::test]
async fn first_or_create_is_alias_for_get_or_create() {
    let pool = make_pool().await;
    let (post, created) = Post::objects()
        .filter("slug", "x".to_string())
        .first_or_create(
            |pool| async move {
                let mut p = Post {
                    id: Auto::default(),
                    slug: "x".into(),
                    title: "Xebra".into(),
                };
                p.save_pool(&pool).await?;
                Ok(p)
            },
            &pool,
        )
        .await
        .unwrap();
    assert!(created);
    assert_eq!(post.title, "Xebra");
}

#[tokio::test]
async fn update_or_create_updates_existing_row() {
    let pool = make_pool().await;
    let mut existing = Post {
        id: Auto::default(),
        slug: "hello".into(),
        title: "Old".into(),
    };
    existing.save_pool(&pool).await.unwrap();

    let (post, created) = Post::objects()
        .filter("slug", "hello".to_string())
        .update_or_create(
            |pool, mut row| async move {
                row.title = "New".into();
                row.save_pool(&pool).await?;
                Ok(row)
            },
            |_pool| async move { panic!("should hit update branch") },
            &pool,
        )
        .await
        .unwrap();
    assert!(!created);
    assert_eq!(post.title, "New");
}

#[tokio::test]
async fn update_or_create_creates_when_missing() {
    let pool = make_pool().await;
    let (post, created) = Post::objects()
        .filter("slug", "fresh".to_string())
        .update_or_create(
            |_pool, _row| async move { panic!("should hit create branch") },
            |pool| async move {
                let mut p = Post {
                    id: Auto::default(),
                    slug: "fresh".into(),
                    title: "Fresh".into(),
                };
                p.save_pool(&pool).await?;
                Ok(p)
            },
            &pool,
        )
        .await
        .unwrap();
    assert!(created);
    assert_eq!(post.title, "Fresh");
}
