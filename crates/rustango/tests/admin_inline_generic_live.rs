//! Live test for `register_admin_inline_generic!` — read-only display
//! of polymorphic children on the parent admin detail page.
//! Issue #242 (slice 3 of epic #246).
//!
//! Mirrors the regular-inline test in `admin_inlines_live.rs` (#50 slice 1),
//! but the child carries `(content_type_id, object_pk)` columns instead
//! of a typed FK, so the WHERE walks ContentType.

#![cfg(feature = "postgres")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::admin::inlines::InlineKind;
use rustango::contenttypes;
use rustango::register_admin_inline_generic;
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
#[rustango(table = "gig_post")]
#[rustango(app = "gig_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gig_tag")]
#[rustango(app = "gig_blog")]
#[rustango(generic_fk(
    name = "content_object",
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

register_admin_inline_generic!(
    parent = "gig_post",
    child = "gig_tag",
    ct = "content_type_id",
    pk = "object_pk",
    kind = InlineKind::Tabular,
    label = "Tags",
    fields = &["name"],
);

async fn fresh(pool: &sqlx::PgPool) {
    let p = rustango::sql::Pool::from(pool.clone());
    contenttypes::ensure_seeded(&p).await.unwrap();
    for t in ["gig_tag", "gig_post"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "gig_post" (
               id BIGSERIAL PRIMARY KEY,
               title VARCHAR(200) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "gig_tag" (
               id BIGSERIAL PRIMARY KEY,
               content_type_id BIGINT NOT NULL,
               object_pk BIGINT NOT NULL,
               name VARCHAR(40) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn parent_detail_renders_generic_inline_panel_with_child_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let p = rustango::sql::Pool::from(pool.clone());

    let mut post = Post {
        id: Auto::Unset,
        title: "Tagged Post".into(),
    };
    post.save_pool(&p).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    // Attach two tags to the post via the typed setter (#240).
    let mut tag_a = Tag {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        name: "rust".into(),
    };
    tag_a
        .set_content_object_for::<Post>(&p, post_pk)
        .await
        .unwrap();
    tag_a.save_pool(&p).await.unwrap();

    let mut tag_b = Tag {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        name: "django-parity".into(),
    };
    tag_b
        .set_content_object_for::<Post>(&p, post_pk)
        .await
        .unwrap();
    tag_b.save_pool(&p).await.unwrap();

    // Also attach a tag to a different Post — must NOT appear in the
    // first post's panel.
    let mut other_post = Post {
        id: Auto::Unset,
        title: "Other".into(),
    };
    other_post.save_pool(&p).await.unwrap();
    let other_pk = *other_post.id.get().unwrap();
    let mut tag_c = Tag {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        name: "other-tag".into(),
    };
    tag_c
        .set_content_object_for::<Post>(&p, other_pk)
        .await
        .unwrap();
    tag_c.save_pool(&p).await.unwrap();

    // GET the parent detail page.
    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/gig_post/{post_pk}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Panel header rendered with the inline's `label`.
    assert!(
        html.contains("Tags"),
        "generic-inline panel header missing: {html}"
    );
    assert!(
        html.contains("class=\"inline-table\""),
        "tabular variant didn't render its <table>: {html}"
    );
    // Both this-post tags visible.
    assert!(html.contains("rust"), "first tag missing: {html}");
    assert!(html.contains("django-parity"), "second tag missing: {html}");
    // The other-post tag must NOT appear — the WHERE pinned both
    // content_type_id and object_pk to this post.
    assert!(
        !html.contains("other-tag"),
        "other-post's tag should not appear: {html}"
    );
    // Edit-link target is the child admin route.
    assert!(
        html.contains("/gig_tag/"),
        "row link should point at child admin route: {html}"
    );
}

#[tokio::test]
async fn parent_detail_renders_empty_state_when_no_generic_children() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let p = rustango::sql::Pool::from(pool.clone());

    let mut post = Post {
        id: Auto::Unset,
        title: "No tags here".into(),
    };
    post.save_pool(&p).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/gig_post/{post_pk}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("Tags"),
        "panel header should still render: {html}"
    );
    assert!(
        html.contains("No related rows"),
        "panel should show empty-state when no generic children: {html}"
    );
}
