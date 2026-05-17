//! Tests for the reverse-direction GenericRelation manager — issue #37.
//! Exercises `fetch_reverse_generic`, `reverse_generic_for`, and
//! `prefetch_reverse_generic_for`. All tests run against in-memory
//! SQLite — no live DB infra needed.

#![cfg(feature = "sqlite")]

use rustango::contenttypes;
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rgr_post")]
#[rustango(app = "rgr_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rgr_article")]
#[rustango(app = "rgr_blog")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rgr_tag")]
#[rustango(app = "rgr_blog")]
#[rustango(generic_fk(
    name = "target",
    ct_column = "content_type_id",
    pk_column = "object_pk"
))]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub content_type_id: i64,
    pub object_pk: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

async fn fresh_pool() -> Pool {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite pool"),
    );
    contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded");
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query(
            "CREATE TABLE rgr_post (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .execute(sq)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE rgr_article (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .execute(sq)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE rgr_tag (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                content_type_id INTEGER NOT NULL, \
                object_pk INTEGER NOT NULL, \
                name TEXT NOT NULL)",
        )
        .execute(sq)
        .await
        .unwrap();
    }
    pool
}

/// Seed a fresh Post + tag it.
async fn seed_post_with_tag(pool: &Pool, post_title: &str, tag_name: &str) -> (i64, i64) {
    let mut p = Post {
        id: Auto::Unset,
        title: post_title.into(),
    };
    p.save_pool(pool).await.unwrap();
    let post_pk = *p.id.get().unwrap();
    let tag_pk = add_tag_for_post(pool, post_pk, tag_name).await;
    (post_pk, tag_pk)
}

/// Tag an existing Post (by pk) without creating a new one.
async fn add_tag_for_post(pool: &Pool, post_pk: i64, tag_name: &str) -> i64 {
    let ct = rustango::contenttypes::ContentType::get_for_model::<Post>(pool)
        .await
        .unwrap()
        .unwrap();
    let ct_id = *ct.id.get().unwrap();

    let mut t = Tag {
        id: Auto::Unset,
        content_type_id: ct_id,
        object_pk: post_pk,
        name: tag_name.into(),
    };
    t.save_pool(pool).await.unwrap();
    *t.id.get().unwrap()
}

#[tokio::test]
async fn fetch_reverse_generic_returns_matching_rows() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let (post_pk, _) = seed_post_with_tag(&pool, "first", "rust").await;
    add_tag_for_post(&pool, post_pk, "web").await;

    let ct = contenttypes::ContentType::get_for_model::<Post>(&pool)
        .await
        .unwrap()
        .unwrap();
    let ct_id = *ct.id.get().unwrap();

    let rows = contenttypes::fetch_reverse_generic(&pool, Tag::SCHEMA, ct_id, post_pk, None)
        .await
        .expect("fetch_reverse_generic");

    assert_eq!(rows.len(), 2, "should find both tags for post #1");
    let names: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    assert!(names.contains("rust"));
    assert!(names.contains("web"));
}

#[tokio::test]
async fn fetch_reverse_generic_filters_by_content_type() {
    // Same object_pk on two different content types should NOT
    // collide — the ct_id filter discriminates.
    contenttypes::clear_cache();
    let pool = fresh_pool().await;

    let mut p = Post {
        id: Auto::Unset,
        title: "post 1".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let post_pk = *p.id.get().unwrap();

    let mut a = Article {
        id: Auto::Unset,
        title: "article 1".into(),
    };
    a.save_pool(&pool).await.unwrap();
    let article_pk = *a.id.get().unwrap();
    // Force the IDs to overlap by inserting until we line them up — in
    // sqlite autoincrement starts at 1 for each table, so post_pk == 1
    // and article_pk == 1.
    assert_eq!(post_pk, article_pk, "test relies on overlapping pks");

    let post_ct = contenttypes::ContentType::get_for_model::<Post>(&pool)
        .await
        .unwrap()
        .unwrap();
    let article_ct = contenttypes::ContentType::get_for_model::<Article>(&pool)
        .await
        .unwrap()
        .unwrap();
    let post_ct_id = *post_ct.id.get().unwrap();
    let article_ct_id = *article_ct.id.get().unwrap();

    // Tag the Post with "for-post"
    let mut tp = Tag {
        id: Auto::Unset,
        content_type_id: post_ct_id,
        object_pk: post_pk,
        name: "for-post".into(),
    };
    tp.save_pool(&pool).await.unwrap();
    // Tag the Article with "for-article"
    let mut ta = Tag {
        id: Auto::Unset,
        content_type_id: article_ct_id,
        object_pk: article_pk,
        name: "for-article".into(),
    };
    ta.save_pool(&pool).await.unwrap();

    // Reverse-fetch for Post: should ONLY get "for-post".
    let rows = contenttypes::fetch_reverse_generic(&pool, Tag::SCHEMA, post_ct_id, post_pk, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("name").and_then(|v| v.as_str()),
        Some("for-post")
    );

    // Reverse-fetch for Article: should ONLY get "for-article".
    let rows =
        contenttypes::fetch_reverse_generic(&pool, Tag::SCHEMA, article_ct_id, article_pk, None)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("name").and_then(|v| v.as_str()),
        Some("for-article")
    );
}

#[tokio::test]
async fn fetch_reverse_generic_returns_empty_for_unmatched_parent() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let _ = seed_post_with_tag(&pool, "first", "rust").await;

    let ct = contenttypes::ContentType::get_for_model::<Post>(&pool)
        .await
        .unwrap()
        .unwrap();
    let ct_id = *ct.id.get().unwrap();

    // pk=99999 — nothing points there.
    let rows = contenttypes::fetch_reverse_generic(&pool, Tag::SCHEMA, ct_id, 99_999, None)
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn reverse_generic_for_resolves_parent_content_type_automatically() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let (post_pk, _) = seed_post_with_tag(&pool, "first", "rust").await;

    // The convenience wrapper resolves the CT from the Parent type.
    let rows = contenttypes::reverse_generic_for::<Post>(&pool, Tag::SCHEMA, post_pk, None)
        .await
        .expect("reverse_generic_for");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("name").and_then(|v| v.as_str()), Some("rust"));
}

#[tokio::test]
async fn prefetch_reverse_generic_groups_children_by_parent_pk() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let (post1_pk, _) = seed_post_with_tag(&pool, "first", "rust").await;
    add_tag_for_post(&pool, post1_pk, "web").await;
    let (post2_pk, _) = seed_post_with_tag(&pool, "second", "django").await;

    let grouped = contenttypes::prefetch_reverse_generic_for::<Post>(
        &pool,
        Tag::SCHEMA,
        &[post1_pk, post2_pk],
        None,
    )
    .await
    .expect("prefetch");

    assert_eq!(grouped.len(), 2);
    let post1_tags: std::collections::HashSet<String> = grouped[&post1_pk]
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    assert!(post1_tags.contains("rust"));
    assert!(post1_tags.contains("web"));

    let post2_tags: std::collections::HashSet<String> = grouped[&post2_pk]
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    assert!(post2_tags.contains("django"));
}

#[tokio::test]
async fn prefetch_reverse_generic_empty_input_yields_empty_map() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let grouped = contenttypes::prefetch_reverse_generic_for::<Post>(&pool, Tag::SCHEMA, &[], None)
        .await
        .unwrap();
    assert!(grouped.is_empty());
}

#[tokio::test]
async fn fetch_reverse_generic_on_model_without_generic_fk_returns_empty() {
    // Post itself doesn't have a generic_fk declared — calling
    // reverse-fetch with Post::SCHEMA as the Child should
    // gracefully return [], not error.
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let rows = contenttypes::fetch_reverse_generic(&pool, Post::SCHEMA, 1, 1, None)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
