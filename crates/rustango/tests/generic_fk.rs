//! Tests for `GenericForeignKey` — issue #36.
//! Exercises `get_object`, `content_type`, `for_target`
//! and the `for_target` cache upgrade (now uses `get_for_model`).
//! All tests run against in-memory SQLite — no infra needed.

#![cfg(feature = "sqlite")]

use rustango::contenttypes::{self, ContentType, GenericForeignKey};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfk_post")]
#[rustango(app = "gfk_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfk_comment")]
#[rustango(app = "gfk_blog")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// FK to rustango_content_types.id — which model this comment is on.
    pub content_type_id: i64,
    /// PK of the target row in the model identified by content_type_id.
    pub object_pk: i64,
    #[rustango(max_length = 500)]
    pub body: String,
}

async fn fresh_pool() -> Pool {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite pool"),
    );
    // Create the content-type catalog first.
    contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded");
    // Create the app tables.
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query(
            "CREATE TABLE gfk_post (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .execute(sq)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE gfk_comment (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                content_type_id INTEGER NOT NULL, \
                object_pk INTEGER NOT NULL, \
                body TEXT NOT NULL)",
        )
        .execute(sq)
        .await
        .unwrap();
    }
    pool
}

/// `GenericForeignKey::for_target` returns a valid GFK pointing at a Post.
#[tokio::test]
async fn for_target_builds_gfk_from_model_type() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;

    // Insert a Post.
    let mut p = Post {
        id: Auto::Unset,
        title: "hello".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let pk = *p.id.get().unwrap();

    let gfk = GenericForeignKey::for_target::<Post>(&pool, pk)
        .await
        .expect("for_target");
    assert_eq!(gfk.object_pk, pk);

    // content_type_id should match the ContentType row for Post.
    let ct = ContentType::get_for_model::<Post>(&pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gfk.content_type_id, *ct.id.get().unwrap());
}

/// `content_type` resolves the ContentType row from the GFK.
#[tokio::test]
async fn content_type_returns_correct_ct() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;

    let ct = ContentType::get_for_model::<Post>(&pool)
        .await
        .unwrap()
        .unwrap();
    let ct_id = *ct.id.get().unwrap();
    let gfk = GenericForeignKey::new(ct_id, 42);

    let resolved = gfk
        .content_type(&pool)
        .await
        .expect("content_type")
        .expect("Some");
    assert_eq!(resolved.table, "gfk_post");
    assert_eq!(resolved.app_label, "gfk_blog");
    assert_eq!(resolved.model_name, "post");
}

/// `content_type` returns `None` for a stale content_type_id.
#[tokio::test]
async fn content_type_returns_none_for_stale_id() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let gfk = GenericForeignKey::new(99_999, 1);
    let ct = gfk.content_type(&pool).await.expect("ok");
    assert!(ct.is_none());
}

/// `get_object` resolves a Post row to a JSON map.
#[tokio::test]
async fn get_object_resolves_target_row_to_json() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;

    // Insert a Post.
    let mut p = Post {
        id: Auto::Unset,
        title: "json target".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let pk = *p.id.get().unwrap();

    let gfk = GenericForeignKey::for_target::<Post>(&pool, pk)
        .await
        .unwrap();
    let obj = gfk
        .get_object(&pool)
        .await
        .expect("get_object")
        .expect("Some");

    // The JSON should have the title field.
    assert_eq!(
        obj.get("title").and_then(|v| v.as_str()),
        Some("json target"),
        "title should be in the JSON: {obj:?}"
    );
}

/// `get_object` returns `None` when the target row doesn't exist.
#[tokio::test]
async fn get_object_returns_none_for_missing_row() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;

    let gfk = GenericForeignKey::for_target::<Post>(&pool, 99_999)
        .await
        .unwrap();
    let obj = gfk.get_object(&pool).await.expect("ok");
    assert!(obj.is_none(), "no row at pk=99999");
}

/// `get_object` returns `None` when content_type_id is stale.
#[tokio::test]
async fn get_object_returns_none_for_stale_content_type_id() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;
    let gfk = GenericForeignKey::new(99_999, 1);
    let obj = gfk.get_object(&pool).await.expect("ok");
    assert!(obj.is_none());
}

/// A Comment row can store a GFK and the target Post is resolved correctly.
/// This is the Django TaggedItem/Comment pattern the issue describes.
#[tokio::test]
async fn comment_with_generic_fk_resolves_post() {
    contenttypes::clear_cache();
    let pool = fresh_pool().await;

    // Insert a Post.
    let mut post = Post {
        id: Auto::Unset,
        title: "the post".into(),
    };
    post.save_pool(&pool).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    // Build the GFK pointing at the Post.
    let gfk = GenericForeignKey::for_target::<Post>(&pool, post_pk)
        .await
        .unwrap();

    // Store as a Comment.
    let mut comment = Comment {
        id: Auto::Unset,
        content_type_id: gfk.content_type_id,
        object_pk: gfk.object_pk,
        body: "great post!".into(),
    };
    comment.save_pool(&pool).await.unwrap();

    // Re-load the comment, reconstruct the GFK, resolve the object.
    let loaded_gfk = GenericForeignKey::new(comment.content_type_id, comment.object_pk);
    let obj = loaded_gfk
        .get_object(&pool)
        .await
        .expect("get_object")
        .expect("Some");
    assert_eq!(obj.get("title").and_then(|v| v.as_str()), Some("the post"));
}
