#![cfg(feature = "sqlite")]
//! Live SQLite tests for the reverse-FK accessor pair (#816).
//!
//! Each FK declaration on a child model auto-emits a `<name>_pool`
//! method on the parent type. The default name is `<child>_set` (Django
//! `<child>_set`); a `#[rustango(default_related_name = "...")]`
//! container attribute on the child overrides it with the
//! caller-supplied identifier.

use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rr_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rr_article", default_related_name = "articles")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub author: ForeignKey<Author>,
}

// Default-name child (no `default_related_name` override) — should
// emit `comment_set_pool` on the parent.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rr_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub body: String,
    pub author: ForeignKey<Author>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE rr_author (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE rr_article (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            author INTEGER NOT NULL REFERENCES rr_author(id)
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE rr_comment (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            body   TEXT NOT NULL,
            author INTEGER NOT NULL REFERENCES rr_author(id)
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) -> (i64, i64) {
    let mut ada = Author {
        id: Auto::default(),
        name: "Ada".into(),
    };
    ada.save_pool(pool).await.unwrap();
    let mut grace = Author {
        id: Auto::default(),
        name: "Grace".into(),
    };
    grace.save_pool(pool).await.unwrap();
    let ada_pk = ada.id.get().copied().unwrap();
    let grace_pk = grace.id.get().copied().unwrap();
    for (title, fk) in [
        ("Engine notes", ada_pk),
        ("Poems on iteration", ada_pk),
        ("Birth of the Bug", grace_pk),
    ] {
        let mut a = Article {
            id: Auto::default(),
            title: title.into(),
            author: ForeignKey::unloaded(fk),
        };
        a.save_pool(pool).await.unwrap();
    }
    for (body, fk) in [
        ("nice work", ada_pk),
        ("agreed", grace_pk),
        ("Also nice", ada_pk),
    ] {
        let mut c = Comment {
            id: Auto::default(),
            body: body.into(),
            author: ForeignKey::unloaded(fk),
        };
        c.save_pool(pool).await.unwrap();
    }
    (ada_pk, grace_pk)
}

#[tokio::test]
async fn override_emits_custom_named_accessor() {
    let pool = make_pool().await;
    let (ada_pk, grace_pk) = seed(&pool).await;

    let ada = QuerySet::<Author>::default()
        .filter("id", ada_pk)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .pop()
        .unwrap();
    // `default_related_name = "articles"` → method is named
    // `articles_pool` on the parent (no `_set` suffix).
    let articles = ada.articles_pool(&pool).await.unwrap();
    assert_eq!(articles.len(), 2);
    let titles: Vec<&str> = articles.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Engine notes"));
    assert!(titles.contains(&"Poems on iteration"));

    let grace = QuerySet::<Author>::default()
        .filter("id", grace_pk)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let articles = grace.articles_pool(&pool).await.unwrap();
    assert_eq!(articles.len(), 1);
    assert_eq!(articles[0].title, "Birth of the Bug");
}

#[tokio::test]
async fn default_name_falls_back_to_child_snake_set() {
    let pool = make_pool().await;
    let (ada_pk, _) = seed(&pool).await;

    let ada = QuerySet::<Author>::default()
        .filter("id", ada_pk)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .pop()
        .unwrap();
    // Comment has no `default_related_name` override → falls back to
    // `<child_snake>_set_pool` → `comment_set_pool`.
    let comments = ada.comment_set_pool(&pool).await.unwrap();
    assert_eq!(comments.len(), 2);
    let bodies: Vec<&str> = comments.iter().map(|c| c.body.as_str()).collect();
    assert!(bodies.contains(&"nice work"));
    assert!(bodies.contains(&"Also nice"));
}

#[tokio::test]
async fn empty_parent_returns_empty_vec() {
    let pool = make_pool().await;
    // No seed — every author has zero articles.
    let mut newcomer = Author {
        id: Auto::default(),
        name: "Newcomer".into(),
    };
    newcomer.save_pool(&pool).await.unwrap();
    let articles = newcomer.articles_pool(&pool).await.unwrap();
    assert_eq!(articles.len(), 0);
}
