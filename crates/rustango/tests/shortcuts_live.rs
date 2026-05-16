#![cfg(all(feature = "postgres", feature = "template_views"))]
//! Live PG tests for `rustango::shortcuts::get_object_or_404` /
//! `get_list_or_404` (issue #10). Verifies the shortcut helpers
//! round-trip through a real database — matching rows return Ok,
//! empty results return Http404.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::shortcuts::{get_list_or_404, get_object_or_404, Http404};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sc_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub published: bool,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "sc_post" CASCADE"#)
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "sc_post" (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(64) NOT NULL,
            published BOOLEAN NOT NULL
        )
        "#,
    )
    .execute(&pg)
    .await
    .unwrap();
    let pool = Pool::Postgres(pg);
    for (title, pub_) in [
        ("Hello World", true),
        ("Draft Post", false),
        ("Second Published", true),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            published: pub_,
        };
        p.insert_pool(&pool).await.unwrap();
    }
    Some(pool)
}

#[tokio::test]
async fn get_object_or_404_returns_match() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let post = get_object_or_404(
        Post::objects().where_(Post::title.eq("Hello World".to_owned())),
        &pool,
    )
    .await
    .expect("post should match");
    assert_eq!(post.title, "Hello World");
}

#[tokio::test]
async fn get_object_or_404_returns_http404_on_no_match() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let err = get_object_or_404(
        Post::objects().where_(Post::title.eq("Nope".to_owned())),
        &pool,
    )
    .await
    .expect_err("should 404");
    // Default message includes the model name.
    assert!(
        err.message.contains("sc_post") || err.message.contains("post"),
        "default 404 message: {err:?}"
    );
}

#[tokio::test]
async fn get_list_or_404_returns_matches() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let posts = get_list_or_404(Post::objects().where_(Post::published.eq(true)), &pool)
        .await
        .expect("two published posts");
    assert_eq!(posts.len(), 2);
}

#[tokio::test]
async fn get_list_or_404_returns_http404_on_empty() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let err = get_list_or_404(
        Post::objects().where_(Post::title.eq("Nonexistent".to_owned())),
        &pool,
    )
    .await
    .expect_err("should 404 on empty");
    let _: &Http404 = &err;
}
