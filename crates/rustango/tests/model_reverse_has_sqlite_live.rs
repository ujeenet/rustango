#![cfg(feature = "sqlite")]
//! Live SQLite tests for the Eloquent `whereHas` / `whereDoesntHave`
//! family — closes issue
//! [#830](https://github.com/ujeenet/rustango/issues/830).
//!
//! `Post hasMany Comment` declared via `#[rustango(reverse_has(...))]`
//! emits two associated functions on `Post`:
//!
//! - `Post::comments_exists_expr() -> WhereExpr` — yields the
//!   correlated `EXISTS (SELECT 1 FROM comment WHERE comment.post_id
//!   = post.id)` subquery for `whereHas`.
//! - `Post::comments_not_exists_expr() -> WhereExpr` — same shape
//!   but `NOT EXISTS`, the `whereDoesntHave` analog.
//!
//! Users drop the result into `QuerySet::where_raw(...)`:
//!
//! ```ignore
//! Post::objects()
//!     .where_raw(Post::comments_exists_expr())
//!     .fetch(&pool).await?;
//! ```
//!
//! Tri-dialect: `EXISTS` is portable across PG / MySQL / SQLite —
//! the IR + writer paths are shared.

use rustango::sql::{sqlx, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rh_post",
    reverse_has(name = "comments", child = "Comment", child_fk_column = "post_id",)
)]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rh_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub body: String,
    pub post_id: ForeignKey<Post, i64>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE rh_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE rh_comment (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            body    TEXT NOT NULL,
            post_id INTEGER NOT NULL REFERENCES rh_post(id)
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

/// Seed three posts. First two have comments; third is bare.
async fn seed(pool: &Pool) -> (i64, i64, i64) {
    let mut p1 = Post {
        id: Auto::default(),
        title: "Has comments".into(),
    };
    p1.save_pool(pool).await.unwrap();
    let p1_id = p1.id.get().copied().unwrap();
    let mut p2 = Post {
        id: Auto::default(),
        title: "Also has one".into(),
    };
    p2.save_pool(pool).await.unwrap();
    let p2_id = p2.id.get().copied().unwrap();
    let mut p3 = Post {
        id: Auto::default(),
        title: "Lonely post".into(),
    };
    p3.save_pool(pool).await.unwrap();
    let p3_id = p3.id.get().copied().unwrap();

    for (post_id, n) in [(p1_id, 3_i32), (p2_id, 1)] {
        for i in 0..n {
            let mut c = Comment {
                id: Auto::default(),
                body: format!("comment-{i}"),
                post_id: ForeignKey::unloaded(post_id),
            };
            c.save_pool(pool).await.unwrap();
        }
    }
    (p1_id, p2_id, p3_id)
}

#[tokio::test]
async fn where_has_returns_only_posts_with_at_least_one_comment() {
    let pool = make_pool().await;
    let (p1_id, p2_id, _p3_id) = seed(&pool).await;

    let mut posts = Post::objects()
        .where_raw(Post::comments_exists_expr())
        .fetch(&pool)
        .await
        .unwrap();
    posts.sort_by_key(|p| p.id.get().copied().unwrap());

    assert_eq!(posts.len(), 2);
    let ids: Vec<i64> = posts.iter().map(|p| p.id.get().copied().unwrap()).collect();
    assert_eq!(ids, vec![p1_id, p2_id]);
}

#[tokio::test]
async fn where_doesnt_have_returns_only_posts_with_zero_comments() {
    let pool = make_pool().await;
    let (_, _, p3_id) = seed(&pool).await;

    let posts = Post::objects()
        .where_raw(Post::comments_not_exists_expr())
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].id.get().copied().unwrap(), p3_id);
    assert_eq!(posts[0].title, "Lonely post");
}

#[tokio::test]
async fn reverse_has_composes_with_user_filters() {
    let pool = make_pool().await;
    let _ = seed(&pool).await;

    // Posts that have comments AND whose title starts with "Has".
    // Only one post matches both: "Has comments".
    let posts = Post::objects()
        .where_raw(Post::comments_exists_expr())
        .filter("title__startswith", "Has")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].title, "Has comments");
}

#[tokio::test]
async fn empty_child_table_returns_zero_via_where_has() {
    let pool = make_pool().await;
    // No seeding — comment table is empty so EXISTS never matches.
    let posts = Post::objects()
        .where_raw(Post::comments_exists_expr())
        .fetch(&pool)
        .await
        .unwrap();
    assert!(posts.is_empty());
}

#[tokio::test]
async fn comments_accessor_returns_chainable_queryset() {
    // Eloquent `$post->comments` analog — bare-name accessor
    // returns a `QuerySet<Comment>` pre-filtered to this post's
    // children. Chainable like any other queryset.
    let pool = make_pool().await;
    let (p1_id, _p2_id, _p3_id) = seed(&pool).await;
    let p1 = Post::find(p1_id, &pool).await.unwrap().unwrap();

    let all = p1.comments().fetch(&pool).await.unwrap();
    assert_eq!(all.len(), 3);
    for c in &all {
        // Every fetched comment's FK matches this post's id.
        assert_eq!(c.post_id.pk(), p1_id);
    }

    // Chain `.filter()` on top of the accessor.
    let filtered = p1
        .comments()
        .filter("body", "comment-1")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].body, "comment-1");
}

#[tokio::test]
async fn comments_pluck_returns_one_column_per_child() {
    // Eloquent `$post->comments->pluck('body')` analog.
    let pool = make_pool().await;
    let (p1_id, _, _) = seed(&pool).await;
    let p1 = Post::find(p1_id, &pool).await.unwrap().unwrap();
    let mut bodies: Vec<String> = p1.comments_pluck("body", &pool).await.unwrap();
    bodies.sort();
    assert_eq!(bodies, vec!["comment-0", "comment-1", "comment-2"]);
}

#[tokio::test]
async fn comments_first_returns_first_child_or_none() {
    // Eloquent `$post->comments->first()` analog — `post.comments_first()`
    // returns Some(first child) or None.
    let pool = make_pool().await;
    let (p1_id, _p2_id, p3_id) = seed(&pool).await;
    let p1 = Post::find(p1_id, &pool).await.unwrap().unwrap();
    let p3 = Post::find(p3_id, &pool).await.unwrap().unwrap();

    let first = p1.comments_first(&pool).await.unwrap();
    assert!(first.is_some());
    assert!(first.unwrap().body.starts_with("comment-"));

    // p3 has no comments → None.
    assert!(p3.comments_first(&pool).await.unwrap().is_none());
}

#[tokio::test]
async fn comments_fetch_bare_name_hot_path() {
    // Bare-name hot path — `post.comments_fetch(&pool)` is the
    // suffix-free shortcut over `post.comments().fetch(&pool)`.
    // Same rows, no `_pool` in the user-visible spelling.
    let pool = make_pool().await;
    let (p1_id, _, _) = seed(&pool).await;
    let p1 = Post::find(p1_id, &pool).await.unwrap().unwrap();
    let rows = p1.comments_fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    for c in &rows {
        assert_eq!(c.post_id.pk(), p1_id);
    }
}

#[tokio::test]
async fn comments_count_returns_per_post_count() {
    // Eloquent `$post->comments->count()` analog — the emitted
    // `comments_count(&pool)` instance method returns the number
    // of comment rows whose `post_id` matches this post.
    let pool = make_pool().await;
    let (p1_id, p2_id, p3_id) = seed(&pool).await;

    let p1 = Post::find(p1_id, &pool).await.unwrap().unwrap();
    let p2 = Post::find(p2_id, &pool).await.unwrap().unwrap();
    let p3 = Post::find(p3_id, &pool).await.unwrap().unwrap();

    assert_eq!(p1.comments_count(&pool).await.unwrap(), 3);
    assert_eq!(p2.comments_count(&pool).await.unwrap(), 1);
    assert_eq!(p3.comments_count(&pool).await.unwrap(), 0);
}
