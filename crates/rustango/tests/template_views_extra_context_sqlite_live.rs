//! `ExtraContext` — per-request template context for the generic views.
//!
//! A generic view builds its context from the model, so a page that
//! `{% extends %}` a shared layout failed the moment that layout read a
//! variable the view never inserts. Middleware now supplies those.

#![cfg(all(feature = "template_views", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::template_views::{ExtraContext, ListView};
use rustango::Model;
use tera::{Context, Tera};
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "xc_post", display = "title")]
#[allow(dead_code)]
pub struct XcPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE xc_post (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO xc_post (id, title) VALUES (1, 'Hello'), (2, 'World')",
        Vec::new(),
    )
    .await
    .expect("seed");
    pool
}

/// A layout reading a variable no generic view knows about, and a list
/// template extending it.
fn tera_with_layout() -> Arc<Tera> {
    let mut tera = Tera::default();
    tera.add_raw_template(
        "base.html",
        "<header>{{ signed_in_as }}</header>{% block content %}{% endblock %}",
    )
    .unwrap();
    tera.add_raw_template(
        "xc_post_list.html",
        r#"{% extends "base.html" %}{% block content %}
           {% for p in object_list %}<li>{{ p.title }}</li>{% endfor %}
           {% endblock %}"#,
    )
    .unwrap();
    Arc::new(tera)
}

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Without the extension the layout's variable is undefined and the
/// render fails — this is the state that made the generic views
/// unusable under a shared layout.
#[tokio::test]
async fn a_layout_variable_the_view_does_not_know_fails_without_it() {
    let pool = fresh_pool().await;
    let app = ListView::for_model(XcPost::SCHEMA).router("/posts", tera_with_layout(), pool);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/posts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "a missing layout variable should not render as a good page"
    );
}

#[tokio::test]
async fn an_extension_supplies_what_the_layout_needs() {
    let pool = fresh_pool().await;
    let mut extra = Context::new();
    extra.insert("signed_in_as", "ada");
    let app = ListView::for_model(XcPost::SCHEMA)
        .router("/posts", tera_with_layout(), pool)
        .layer(axum::Extension(ExtraContext(extra)));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/posts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_of(resp).await;
    assert!(html.contains("ada"), "the layout variable: {html}");
    assert!(html.contains("Hello"), "and the rows still render: {html}");
}

/// The view owns its own keys: an extension cannot replace the rows.
#[tokio::test]
async fn the_view_wins_on_a_key_collision() {
    let pool = fresh_pool().await;
    let mut extra = Context::new();
    extra.insert("signed_in_as", "ada");
    extra.insert("object_list", &Vec::<String>::new());
    let app = ListView::for_model(XcPost::SCHEMA)
        .router("/posts", tera_with_layout(), pool)
        .layer(axum::Extension(ExtraContext(extra)));

    let html = body_of(
        app.oneshot(
            Request::builder()
                .uri("/posts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(
        html.contains("Hello"),
        "an extension must not be able to blank the rows: {html}"
    );
}

/// The pager range comes from the framework's own `Paginator`, so a
/// long list renders `1 … 7 8 9 … 42` rather than one link per page.
#[tokio::test]
async fn the_list_exposes_an_elided_page_range() {
    let pool = fresh_pool().await;
    let mut tera = Tera::default();
    tera.add_raw_template(
        "xc_post_list.html",
        "{% for m in page_marks %}{% if m.ellipsis %}…{% else %}[{{ m.number }}]{% endif %}{% endfor %}",
    )
    .unwrap();
    let app =
        ListView::for_model(XcPost::SCHEMA)
            .page_size(1)
            .router("/posts", Arc::new(tera), pool);

    let html = body_of(
        app.oneshot(
            Request::builder()
                .uri("/posts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(html.contains("[1]"), "page 1 should be marked: {html}");
    assert!(html.contains("[2]"), "and page 2 exists: {html}");
}
