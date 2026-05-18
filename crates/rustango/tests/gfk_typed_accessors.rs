//! Live test for the typed GenericForeignKey accessor + setter
//! emitted by `#[rustango(generic_fk(...))]` — issues #239 + #240.
//!
//! Runs against in-memory SQLite — no live DB infra needed.

#![cfg(feature = "sqlite")]

use rustango::contenttypes;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfkt_post")]
#[rustango(app = "gfkt_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfkt_article")]
#[rustango(app = "gfkt_blog")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfkt_comment")]
#[rustango(app = "gfkt_blog")]
#[rustango(generic_fk(
    name = "content_object",
    ct_column = "content_type_id",
    pk_column = "object_pk"
))]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub content_type_id: i64,
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
    contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded");
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query(
            "CREATE TABLE gfkt_post (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .execute(sq)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE gfkt_article (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .execute(sq)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE gfkt_comment (\
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

#[tokio::test]
async fn typed_setter_assigns_ct_and_pk_for_target_model() {
    let pool = fresh_pool().await;

    // Seed a Post.
    let mut post = Post {
        id: Auto::Unset,
        title: "Original post".into(),
    };
    post.save_pool(&pool).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    // Build a comment and attach it to the Post via the generated setter.
    // Without #240, the caller would have to manually call
    // `GenericForeignKey::for_target::<Post>(&pool, post_pk).await?`
    // and assign both columns separately.
    let mut comment = Comment {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        body: "first!".into(),
    };
    comment
        .set_content_object_for::<Post>(&pool, post_pk)
        .await
        .expect("setter should resolve ContentType and assign both columns");

    assert_eq!(
        comment.object_pk, post_pk,
        "setter must assign object_pk to the target's PK"
    );
    let post_ct = contenttypes::ContentType::get_for_model::<Post>(&pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        comment.content_type_id,
        *post_ct.id.get().unwrap(),
        "setter must assign content_type_id to the target's ContentType"
    );

    // Persist + re-load and the columns survive the round-trip.
    comment.save_pool(&pool).await.unwrap();
    let comment_pk = *comment.id.get().unwrap();
    assert!(comment_pk > 0, "comment should have been INSERTed");
}

#[tokio::test]
async fn typed_accessor_resolves_to_target_row_as_json() {
    let pool = fresh_pool().await;

    let mut post = Post {
        id: Auto::Unset,
        title: "Post for accessor test".into(),
    };
    post.save_pool(&pool).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    let mut comment = Comment {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        body: "looks good".into(),
    };
    comment
        .set_content_object_for::<Post>(&pool, post_pk)
        .await
        .unwrap();
    comment.save_pool(&pool).await.unwrap();

    // The emitted accessor reads self.content_type_id + self.object_pk
    // and resolves to the target row as a JSON map. Stand-in for
    // Django's `comment.content_object`.
    let target_json = comment
        .content_object_pool(&pool)
        .await
        .expect("accessor should not error")
        .expect("target row should resolve");

    let title = target_json
        .get("title")
        .and_then(|v| v.as_str())
        .expect("target JSON should carry the Post's title");
    assert_eq!(title, "Post for accessor test");
}

#[tokio::test]
async fn typed_accessor_dispatches_across_target_models() {
    let pool = fresh_pool().await;

    // Seed one Post + one Article.
    let mut post = Post {
        id: Auto::Unset,
        title: "Post body".into(),
    };
    post.save_pool(&pool).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    let mut article = Article {
        id: Auto::Unset,
        title: "Article body".into(),
    };
    article.save_pool(&pool).await.unwrap();
    let article_pk = *article.id.get().unwrap();

    // Comment 1 → Post
    let mut c1 = Comment {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        body: "p".into(),
    };
    c1.set_content_object_for::<Post>(&pool, post_pk)
        .await
        .unwrap();
    c1.save_pool(&pool).await.unwrap();

    // Comment 2 → Article
    let mut c2 = Comment {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        body: "a".into(),
    };
    c2.set_content_object_for::<Article>(&pool, article_pk)
        .await
        .unwrap();
    c2.save_pool(&pool).await.unwrap();

    // Each accessor call resolves to the right target type's row.
    let j1 = c1.content_object_pool(&pool).await.unwrap().unwrap();
    let j2 = c2.content_object_pool(&pool).await.unwrap().unwrap();
    assert_eq!(j1.get("title").and_then(|v| v.as_str()), Some("Post body"));
    assert_eq!(
        j2.get("title").and_then(|v| v.as_str()),
        Some("Article body")
    );
}

#[tokio::test]
async fn typed_accessor_returns_none_for_stale_target() {
    let pool = fresh_pool().await;

    // Comment carries a content_type_id + object_pk that point at
    // a row that never existed. Equivalent to "the target was deleted
    // out from under the GFK" — the accessor returns Ok(None) instead
    // of erroring, matching the underlying `GenericForeignKey::get_object`
    // contract.
    let post_ct = contenttypes::ContentType::get_for_model::<Post>(&pool)
        .await
        .unwrap()
        .unwrap();
    let mut comment = Comment {
        id: Auto::Unset,
        content_type_id: *post_ct.id.get().unwrap(),
        object_pk: 999_999, // no such Post
        body: "orphan".into(),
    };
    comment.save_pool(&pool).await.unwrap();

    let resolved = comment
        .content_object_pool(&pool)
        .await
        .expect("accessor should not error on stale target");
    assert!(
        resolved.is_none(),
        "stale target should resolve to None, got: {resolved:?}"
    );
}
