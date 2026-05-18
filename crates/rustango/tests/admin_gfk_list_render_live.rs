//! Live test for admin list-view rendering of `#[rustango(generic_fk)]`
//! columns as clickable target links. Issue #241.
//!
//! Seeds a Comment model carrying `(content_type_id, object_pk)`
//! pointing at two different target tables (Post + Article), declares
//! `list_display = "body, content_object"`, then GETs the list page
//! and asserts the `content_object` column renders an `<a href>` for
//! each row.

#![cfg(feature = "postgres")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::contenttypes;
use rustango::sql::sqlx;
use rustango::sql::Auto;
use rustango::Model;
use tower::ServiceExt;

use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfklist_post")]
#[rustango(app = "gfklist_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfklist_article")]
#[rustango(app = "gfklist_blog")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfklist_comment")]
#[rustango(app = "gfklist_blog")]
#[rustango(generic_fk(
    name = "content_object",
    ct_column = "content_type_id",
    pk_column = "object_pk"
))]
// list_display includes the GFK relation by its `name` —
// `content_object` collapses the (ct_column, pk_column) pair into a
// single clickable cell.
#[rustango(admin(list_display = "body, content_object"))]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub content_type_id: i64,
    pub object_pk: i64,
    #[rustango(max_length = 500)]
    pub body: String,
}

async fn fresh(pool: &sqlx::PgPool) {
    let p = rustango::sql::Pool::from(pool.clone());
    contenttypes::ensure_seeded(&p).await.unwrap();
    for t in ["gfklist_comment", "gfklist_article", "gfklist_post"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "gfklist_post" (
               id BIGSERIAL PRIMARY KEY,
               title VARCHAR(200) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "gfklist_article" (
               id BIGSERIAL PRIMARY KEY,
               title VARCHAR(200) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "gfklist_comment" (
               id BIGSERIAL PRIMARY KEY,
               content_type_id BIGINT NOT NULL,
               object_pk BIGINT NOT NULL,
               body VARCHAR(500) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn list_view_renders_gfk_pair_as_single_clickable_cell() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let p = rustango::sql::Pool::from(pool.clone());

    // Seed one Post + one Article.
    let mut post = Post {
        id: Auto::Unset,
        title: "Post target".into(),
    };
    post.save_pool(&p).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    let mut article = Article {
        id: Auto::Unset,
        title: "Article target".into(),
    };
    article.save_pool(&p).await.unwrap();
    let article_pk = *article.id.get().unwrap();

    // Comment 1 → Post, Comment 2 → Article. Uses the typed setter
    // from #240 to populate (content_type_id, object_pk).
    let mut c1 = Comment {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        body: "comment on post".into(),
    };
    c1.set_content_object_for::<Post>(&p, post_pk)
        .await
        .unwrap();
    c1.save_pool(&p).await.unwrap();

    let mut c2 = Comment {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        body: "comment on article".into(),
    };
    c2.set_content_object_for::<Article>(&p, article_pk)
        .await
        .unwrap();
    c2.save_pool(&p).await.unwrap();

    // GET the comment list view.
    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri("/gfklist_comment")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // The `content_object` column header is rendered (matches the
    // generic_fk `name`).
    assert!(
        html.contains(">content_object<"),
        "GFK column header missing: {html}"
    );
    // Comment 1 → Post link.
    let post_link = format!(r#"<a href="/gfklist_post/{post_pk}">"#);
    assert!(
        html.contains(&post_link),
        "Post link missing for comment 1: looked for `{post_link}` in: {html}"
    );
    assert!(
        html.contains(&format!("gfklist_blog.post #{post_pk}")),
        "Post label missing: {html}"
    );
    // Comment 2 → Article link.
    let article_link = format!(r#"<a href="/gfklist_article/{article_pk}">"#);
    assert!(
        html.contains(&article_link),
        "Article link missing for comment 2: looked for `{article_link}` in: {html}"
    );
    assert!(
        html.contains(&format!("gfklist_blog.article #{article_pk}")),
        "Article label missing: {html}"
    );
    // Neither the raw ct_column nor the raw pk_column should appear as
    // standalone header cells — the GFK declaration folds them into
    // the single `content_object` column.
    assert!(
        !html.contains(">content_type_id<"),
        "raw ct_column header should not render when GFK folds the pair: {html}"
    );
    assert!(
        !html.contains(">object_pk<"),
        "raw pk_column header should not render when GFK folds the pair: {html}"
    );
}

#[tokio::test]
async fn list_view_falls_back_to_placeholder_on_stale_content_type() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // Insert a comment carrying a ct_id that's NOT seeded — simulates
    // "the source app got uninstalled but the comment row stuck around".
    sqlx::query(
        r#"INSERT INTO "gfklist_comment" (content_type_id, object_pk, body)
           VALUES (999999, 42, 'orphan')"#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri("/gfklist_comment")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Stale CT renders as `(ct=N, pk=M)` placeholder, same shape as
    // contenttypes::render_generic_fk_link's fallback.
    assert!(
        html.contains("(ct=999999, pk=42)"),
        "stale-CT fallback missing: {html}"
    );
}
