#![cfg(feature = "postgres")]
//! Live test for slice 9.0e — `fetch_with_prefetch::<Author, Post>(...)`
//! returns `Vec<(Author, Vec<Post>)>` from **two** SQL queries flat:
//! one over the parent, one batched over the children via `WHERE
//! <fk_column> IN (...)`. Each parent paired with its matching
//! children.
//!
//! Skipped silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::sql::{fetch_with_prefetch, sqlx, Auto, ForeignKey};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_pf_author", display = "name")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_pf_post", display = "title")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub author: ForeignKey<Author>,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn setup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_pf_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_pf_author" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rustango_pf_author" (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rustango_pf_post" (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(200) NOT NULL,
            author BIGINT NOT NULL REFERENCES "rustango_pf_author"(id)
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn prefetch_groups_children_under_parents() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup(&pool).await;

    let mut ada = Author {
        id: Auto::default(),
        name: "Ada".into(),
    };
    ada.save(&pool).await.unwrap();
    let mut grace = Author {
        id: Auto::default(),
        name: "Grace".into(),
    };
    grace.save(&pool).await.unwrap();
    let mut linus = Author {
        id: Auto::default(),
        name: "Linus".into(),
    };
    linus.save(&pool).await.unwrap(); // no posts — should still appear with empty Vec

    let ada_pk = ada.id.get().copied().unwrap();
    let grace_pk = grace.id.get().copied().unwrap();

    for (title, fk) in [
        ("Analytical Engine", ada_pk),
        ("Algorithms as Poetry", ada_pk),
        ("Birth of the Bug", grace_pk),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            author: ForeignKey::unloaded(fk),
        };
        p.save(&pool).await.unwrap();
    }

    let groups: Vec<(Author, Vec<Post>)> =
        fetch_with_prefetch::<Author, Post>(Author::objects(), "author", &pool)
            .await
            .unwrap();
    assert_eq!(groups.len(), 3);

    // Ada should have 2, Grace 1, Linus 0.
    let by_name: std::collections::HashMap<&str, usize> = groups
        .iter()
        .map(|(a, kids)| (a.name.as_str(), kids.len()))
        .collect();
    assert_eq!(by_name.get("Ada"), Some(&2));
    assert_eq!(by_name.get("Grace"), Some(&1));
    assert_eq!(by_name.get("Linus"), Some(&0));

    // Verify each child's FK actually points at its parent.
    for (parent, kids) in &groups {
        let parent_pk = parent.id.get().copied().unwrap();
        for k in kids {
            assert_eq!(k.author.pk(), parent_pk);
        }
    }
}

#[tokio::test]
async fn prefetch_with_no_parents_returns_empty() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup(&pool).await;
    let groups: Vec<(Author, Vec<Post>)> =
        fetch_with_prefetch::<Author, Post>(Author::objects(), "author", &pool)
            .await
            .unwrap();
    assert!(groups.is_empty());
}
