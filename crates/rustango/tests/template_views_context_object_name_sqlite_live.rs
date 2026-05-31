//! Django-parity #379 — `ListView::context_object_name` /
//! `DetailView::context_object_name` / `DetailView::lookup_field`.
//! Verifies the Tera context picks up the renamed binding and
//! that `lookup_field` lets the DetailView probe by a non-PK
//! column.

#![cfg(all(feature = "template_views", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::template_views::{DetailView, ListView};
use rustango::Model;
use tera::Tera;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ctx_post", display = "title")]
#[allow(dead_code)]
pub struct CtxPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 200, unique)]
    pub slug: String,
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE ctx_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            slug  TEXT NOT NULL UNIQUE
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO ctx_post (id, title, slug) VALUES (1, 'Hello', 'hello'), (2, 'World', 'world')",
        Vec::new(),
    )
    .await
    .expect("seed");
    pool
}

fn tera_with(name: &str, body: &str) -> Arc<Tera> {
    let mut t = Tera::default();
    t.add_raw_template(name, body).unwrap();
    Arc::new(t)
}

async fn body_of(app: axum::Router, path: &str) -> (StatusCode, String) {
    let resp = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn list_view_binds_context_object_name_alongside_object_list() {
    let pool = fresh_pool().await;
    let tera = tera_with(
        "ctx_post_list.html",
        "alt={{ posts | length }} legacy={{ object_list | length }}",
    );
    let view = ListView::for_model(CtxPost::SCHEMA).context_object_name("posts");
    let app = view.router("/posts", tera, pool);
    let (status, body) = body_of(app, "/posts").await;
    assert_eq!(status, StatusCode::OK);
    // Both bindings should resolve to 2 rows.
    assert!(
        body.contains("alt=2"),
        "renamed binding missing, got {body}"
    );
    assert!(
        body.contains("legacy=2"),
        "default object_list still wired for back-compat, got {body}"
    );
}

#[tokio::test]
async fn list_view_without_context_object_name_only_binds_object_list() {
    let pool = fresh_pool().await;
    // `posts` here is NOT defined; default(value="MISSING") catches
    // it. Tera evaluates undefined variable references against
    // `into_json` and would raise if not guarded.
    let tera = tera_with(
        "ctx_post_list.html",
        r#"alt={{ posts | default(value="MISSING") }} legacy={{ object_list | length }}"#,
    );
    let view = ListView::for_model(CtxPost::SCHEMA);
    let app = view.router("/posts", tera, pool);
    let (status, body) = body_of(app, "/posts").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("alt=MISSING"), "got {body}");
    assert!(body.contains("legacy=2"), "got {body}");
}

#[tokio::test]
async fn detail_view_binds_context_object_name_alongside_object() {
    let pool = fresh_pool().await;
    let tera = tera_with(
        "ctx_post_detail.html",
        "alt={{ post.title | safe }} legacy={{ object.title | safe }}",
    );
    let view = DetailView::for_model(CtxPost::SCHEMA).context_object_name("post");
    let app = view.router("/posts", tera, pool);
    let (status, body) = body_of(app, "/posts/1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("alt=Hello"), "got {body}");
    assert!(body.contains("legacy=Hello"), "got {body}");
}

#[tokio::test]
async fn detail_view_lookup_field_probes_by_named_column() {
    let pool = fresh_pool().await;
    let tera = tera_with(
        "ctx_post_detail.html",
        "row={{ object.title | safe }} slug={{ object.slug | safe }}",
    );
    let view = DetailView::for_model(CtxPost::SCHEMA).lookup_field("slug");
    let app = view.router("/posts", tera, pool);
    // URL `/posts/world` probes WHERE slug = 'world' → returns row id=2.
    let (status, body) = body_of(app, "/posts/world").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("row=World"), "got {body}");
    assert!(body.contains("slug=world"), "got {body}");
}

#[tokio::test]
async fn detail_view_lookup_field_unknown_column_returns_500() {
    let pool = fresh_pool().await;
    let tera = tera_with("ctx_post_detail.html", "ignored");
    let view = DetailView::for_model(CtxPost::SCHEMA).lookup_field("not_a_real_field");
    let app = view.router("/posts", tera, pool);
    let (status, body) = body_of(app, "/posts/anything").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body.contains("not_a_real_field"),
        "error body should name the offending field, got: {body}"
    );
}

#[tokio::test]
async fn detail_view_missing_row_returns_404_regardless_of_lookup_field() {
    let pool = fresh_pool().await;
    let tera = tera_with("ctx_post_detail.html", "ignored");
    let view = DetailView::for_model(CtxPost::SCHEMA).lookup_field("slug");
    let app = view.router("/posts", tera, pool);
    let (status, _) = body_of(app, "/posts/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
