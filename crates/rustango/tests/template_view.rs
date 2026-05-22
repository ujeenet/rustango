//! Django-parity #377 — `TemplateView` class-based view.
//!
//! Verifies `template_views::TemplateView` renders a Tera template
//! with caller-supplied context and mounts as an axum router on the
//! configured prefix.

#![cfg(all(feature = "sqlite", feature = "template_views"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::template_views::TemplateView;
use serde_json::json;
use tera::Tera;
use tower::ServiceExt;

fn build_tera() -> Arc<Tera> {
    let mut tera = Tera::default();
    tera.add_raw_template(
        "tv_hello.html",
        "<h1>Hello, {{ name }}!</h1><p>{{ msg }}</p>",
    )
    .expect("add template");
    Arc::new(tera)
}

#[tokio::test]
async fn template_view_renders_context_value() {
    let tera = build_tera();
    let app = TemplateView::new("tv_hello.html")
        .context_value("name", "Alice")
        .context_value("msg", "Welcome.")
        .router("/hello", tera);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body_bytes).unwrap();
    assert!(body.contains("Hello, Alice!"), "missing greeting: {body}");
    assert!(body.contains("Welcome."), "missing msg: {body}");
}

#[tokio::test]
async fn template_view_accepts_json_object_context() {
    let tera = build_tera();
    let app = TemplateView::new("tv_hello.html")
        .context(json!({ "name": "Bob", "msg": "from JSON" }))
        .router("/hi", tera);

    let resp = app
        .oneshot(Request::builder().uri("/hi").body(Body::empty()).unwrap())
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body_bytes).unwrap();
    assert!(body.contains("Hello, Bob!"));
    assert!(body.contains("from JSON"));
}

#[tokio::test]
async fn template_view_later_context_value_wins() {
    let tera = build_tera();
    let app = TemplateView::new("tv_hello.html")
        .context_value("name", "first")
        .context_value("msg", "ignored")
        .context_value("name", "winner")
        .router("/win", tera);

    let resp = app
        .oneshot(Request::builder().uri("/win").body(Body::empty()).unwrap())
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body_bytes).unwrap();
    assert!(body.contains("Hello, winner!"));
}
