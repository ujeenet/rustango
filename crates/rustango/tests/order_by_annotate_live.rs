//! Live tests for slice 9.0b — `QuerySet::order_by(&[(field, desc)])`
//! emits `ORDER BY` and `annotate_count_children` returns
//! `Vec<(Parent, i64)>` from a single SELECT-with-GROUP-BY.
//!
//! Skipped silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::sql::{annotate_count_children, annotate_count_children_on, sqlx, Auto, ForeignKey};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_ob_author", display = "name")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_ob_post", display = "title")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub author: ForeignKey<Author>,
    pub published_at: chrono::DateTime<chrono::Utc>,
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
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_ob_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_ob_author" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rustango_ob_author" (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rustango_ob_post" (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(200) NOT NULL,
            author BIGINT NOT NULL REFERENCES "rustango_ob_author"(id),
            published_at TIMESTAMPTZ NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn order_by_descending_published_at() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup(&pool).await;

    let mut a = Author {
        id: Auto::default(),
        name: "Solo".into(),
    };
    a.save(&pool).await.unwrap();
    let a_pk = a.id.get().copied().unwrap();

    let now = chrono::Utc::now();
    for (title, hours_ago) in [("oldest", 5_i64), ("middle", 3), ("newest", 1)] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            author: ForeignKey::unloaded(a_pk),
            published_at: now - chrono::Duration::hours(hours_ago),
        };
        p.save(&pool).await.unwrap();
    }

    let posts: Vec<Post> = Post::objects()
        .order_by(&[("published_at", true)])
        .fetch_on(&pool)
        .await
        .unwrap();
    let titles: Vec<&str> = posts.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, vec!["newest", "middle", "oldest"]);
}

#[tokio::test]
async fn annotate_count_children_groups_per_parent() {
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
    linus.save(&pool).await.unwrap();

    let ada_pk = ada.id.get().copied().unwrap();
    let grace_pk = grace.id.get().copied().unwrap();

    let now = chrono::Utc::now();
    for (fk, _) in [(ada_pk, 0), (ada_pk, 1), (ada_pk, 2), (grace_pk, 3)] {
        let mut p = Post {
            id: Auto::default(),
            title: "x".into(),
            author: ForeignKey::unloaded(fk),
            published_at: now,
        };
        p.save(&pool).await.unwrap();
    }

    let counts: Vec<(Author, i64)> = annotate_count_children::<Author>(
        Author::objects(),
        "rustango_ob_post",
        "author",
        &pool,
    )
    .await
    .unwrap();
    assert_eq!(counts.len(), 3);
    let by_name: std::collections::HashMap<&str, i64> = counts
        .iter()
        .map(|(a, n)| (a.name.as_str(), *n))
        .collect();
    assert_eq!(by_name.get("Ada"), Some(&3));
    assert_eq!(by_name.get("Grace"), Some(&1));
    assert_eq!(by_name.get("Linus"), Some(&0));
}

#[tokio::test]
async fn annotate_count_children_on_works_against_acquired_connection() {
    // S4 regression: tenant-scoped admin/api code drives the ORM
    // through a `&mut PgConnection` (search_path scoped to a tenant
    // schema). Before v0.9.1 only `&PgPool` was accepted, forcing a
    // per-parent `count_on` N+1. The `_on` variant must produce the
    // same Vec<(Parent, i64)> as the pool variant.
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

    let now = chrono::Utc::now();
    for fk in [
        ada.id.get().copied().unwrap(),
        ada.id.get().copied().unwrap(),
        grace.id.get().copied().unwrap(),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: "x".into(),
            author: ForeignKey::unloaded(fk),
            published_at: now,
        };
        p.save(&pool).await.unwrap();
    }

    let mut conn = pool.acquire().await.unwrap();
    let counts: Vec<(Author, i64)> =
        annotate_count_children_on::<Author, _>(
            Author::objects(),
            "rustango_ob_post",
            "author",
            &mut *conn,
        )
        .await
        .unwrap();
    let by_name: std::collections::HashMap<&str, i64> = counts
        .iter()
        .map(|(a, n)| (a.name.as_str(), *n))
        .collect();
    assert_eq!(by_name.get("Ada"), Some(&2));
    assert_eq!(by_name.get("Grace"), Some(&1));
}

#[tokio::test]
async fn order_by_unknown_field_errors() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup(&pool).await;
    let result = Post::objects()
        .order_by(&[("nonexistent", false)])
        .fetch_on(&pool)
        .await;
    assert!(result.is_err());
}
