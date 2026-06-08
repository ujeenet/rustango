#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::where_in_subquery(col, inner)` /
//! `QuerySet::where_not_in_subquery(col, inner)` — Eloquent
//! `Builder::whereIn(\$col, \$closure)` / `whereNotIn(\$col, \$closure)`
//! parity. Routes through the existing `subquery::in_subquery` /
//! `not_in_subquery` free functions.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "wis_category")]
#[allow(dead_code)]
pub struct Category {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub is_public: bool,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "wis_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub category_id: i64,
    #[rustango(max_length = 40)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE wis_category (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            is_public INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE wis_post (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            category_id INTEGER NOT NULL,
            title       TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) {
    let mut pub_cat = Category {
        id: Auto::default(),
        is_public: true,
    };
    pub_cat.save_pool(pool).await.unwrap();
    let pub_id = *pub_cat.id.get().unwrap();
    let mut priv_cat = Category {
        id: Auto::default(),
        is_public: false,
    };
    priv_cat.save_pool(pool).await.unwrap();
    let priv_id = *priv_cat.id.get().unwrap();
    for (cat_id, title) in [
        (pub_id, "alpha"),
        (priv_id, "bravo"),
        (pub_id, "charlie"),
        (priv_id, "delta"),
    ] {
        let mut p = Post {
            id: Auto::default(),
            category_id: cat_id,
            title: title.into(),
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_in_subquery_finds_posts_with_public_category() {
    let pool = make_pool().await;
    seed(&pool).await;
    let public_ids = Category::objects()
        .filter("is_public", true)
        .values_list_flat("id")
        .compile()
        .unwrap();
    let mut titles: Vec<String> = Post::objects()
        .where_in_subquery("category_id", public_ids)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.title)
        .collect();
    titles.sort();
    assert_eq!(titles, vec!["alpha", "charlie"]);
}

#[tokio::test]
async fn where_not_in_subquery_finds_posts_outside_public_category() {
    let pool = make_pool().await;
    seed(&pool).await;
    let public_ids = Category::objects()
        .filter("is_public", true)
        .values_list_flat("id")
        .compile()
        .unwrap();
    let mut titles: Vec<String> = Post::objects()
        .where_not_in_subquery("category_id", public_ids)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.title)
        .collect();
    titles.sort();
    assert_eq!(titles, vec!["bravo", "delta"]);
}
