#![cfg(feature = "sqlite")]
//! `fetch_with_prefetch_filtered` — closes #298 / T2.1.
//!
//! Live SQLite test for Django's `Prefetch(queryset=...)` shape:
//! the caller supplies a child `QuerySet` carrying the filters /
//! ordering they want, and the prefetch helper injects the FK-IN
//! predicate so each parent picks up only its matching children.

use rustango::query::QuerySet;
use rustango::sql::{fetch_with_prefetch_filtered, sqlx, Auto, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "pff_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pff_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub published: bool,
    pub created: i64,
    pub author: ForeignKey<Author>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE pff_author (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE pff_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            published INTEGER NOT NULL,
            created   INTEGER NOT NULL,
            author    INTEGER NOT NULL REFERENCES pff_author(id)
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
    // Ada: 2 published, 1 draft. Grace: 1 published.
    for (title, published, created, fk) in [
        ("Engine notes", true, 100, ada_pk),
        ("Poems on iteration", true, 200, ada_pk),
        ("Draft on tape decks", false, 150, ada_pk),
        ("Birth of the Bug", true, 300, grace_pk),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            published,
            created,
            author: ForeignKey::unloaded(fk),
        };
        p.save_pool(pool).await.unwrap();
    }
    (ada_pk, grace_pk)
}

#[tokio::test]
async fn filtered_prefetch_skips_unpublished_children() {
    let pool = make_pool().await;
    let _ = seed(&pool).await;

    // Child queryset: only published posts.
    let child_qs = QuerySet::<Post>::default().filter("published", true);

    let groups: Vec<(Author, Vec<Post>)> =
        fetch_with_prefetch_filtered::<Author, Post>(Author::objects(), "author", child_qs, &pool)
            .await
            .expect("prefetch filtered");

    assert_eq!(groups.len(), 2, "two authors");
    for (author, posts) in &groups {
        for p in posts {
            assert!(
                p.published,
                "unpublished post leaked through filtered prefetch for {}: {}",
                author.name, p.title
            );
        }
    }
    // Ada had 2 published; Grace had 1.
    let by_name: std::collections::HashMap<String, usize> = groups
        .iter()
        .map(|(a, ps)| (a.name.clone(), ps.len()))
        .collect();
    assert_eq!(by_name.get("Ada").copied(), Some(2));
    assert_eq!(by_name.get("Grace").copied(), Some(1));
}

#[tokio::test]
async fn filtered_prefetch_preserves_user_order_by() {
    let pool = make_pool().await;
    let _ = seed(&pool).await;

    // Child queryset: published, ordered by created ASC.
    let child_qs = QuerySet::<Post>::default()
        .filter("published", true)
        .order_by(&[("created", false)]);

    let groups: Vec<(Author, Vec<Post>)> =
        fetch_with_prefetch_filtered::<Author, Post>(Author::objects(), "author", child_qs, &pool)
            .await
            .expect("prefetch filtered");

    // Find Ada's group; her published children should be ordered by
    // `created` ASC: Engine notes (100) before Poems on iteration (200).
    let ada_group = groups
        .iter()
        .find(|(a, _)| a.name == "Ada")
        .expect("Ada present");
    let titles: Vec<&str> = ada_group.1.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["Engine notes", "Poems on iteration"],
        "child order_by must survive the IN-injection: {titles:?}"
    );
}

#[tokio::test]
async fn unfiltered_child_queryset_falls_back_to_every_child() {
    // Equivalent to v0.41's plain fetch_with_prefetch_pool — the
    // filtered variant with an empty child_qs IS a superset.
    let pool = make_pool().await;
    let _ = seed(&pool).await;

    let child_qs = QuerySet::<Post>::default(); // no filters

    let groups: Vec<(Author, Vec<Post>)> =
        fetch_with_prefetch_filtered::<Author, Post>(Author::objects(), "author", child_qs, &pool)
            .await
            .expect("prefetch filtered");

    let by_name: std::collections::HashMap<String, usize> = groups
        .iter()
        .map(|(a, ps)| (a.name.clone(), ps.len()))
        .collect();
    // Ada: 3 (2 published + 1 draft). Grace: 1.
    assert_eq!(by_name.get("Ada").copied(), Some(3));
    assert_eq!(by_name.get("Grace").copied(), Some(1));
}
