//! Issue #377 — the `TemplateView` generic view.
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

/// A page may hold a form posting elsewhere, so it gets the CSRF token.
#[tokio::test]
async fn template_view_stamps_the_csrf_token() {
    let mut tera = Tera::default();
    tera.add_raw_template("tv_csrf.html", "{{ csrf_input | safe }}|{{ csrf_token }}")
        .unwrap();
    let app = TemplateView::new("tv_csrf.html").router("/c", Arc::new(tera));
    let resp = app
        .oneshot(Request::builder().uri("/c").body(Body::empty()).unwrap())
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body_bytes).unwrap();
    assert!(body.contains("name=\"_csrf\""), "{body}");
}

#[derive(rustango::Form, Default, Debug)]
#[form(validate = "check_pair")]
pub struct PairForm {
    pub start: i64,
    pub end: i64,
}

impl PairForm {
    fn check_pair(&self) -> Result<(), rustango::forms::FormErrors> {
        let mut errs = rustango::forms::FormErrors::default();
        if self.start > self.end {
            errs.add_non_field("start must be at most end");
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

/// A form-wide error from `#[form(validate)]` reaches the page as a 422.
#[tokio::test]
async fn form_view_shows_form_wide_errors() {
    const CSRF: &str = "tv-form-token-tv-form-token-tv-form-token-x";
    let mut tera = Tera::default();
    tera.add_raw_template("tv_pair.html", "{{ errors.__all__ | join(sep=\";\") }}")
        .unwrap();
    let app = rustango::template_views::FormView::<PairForm>::for_form(|_f| async { Ok(()) })
        .template("tv_pair.html")
        .router("/pair", Arc::new(tera));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/pair")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", format!("rustango_csrf={CSRF}"))
                .body(Body::from(format!("_csrf={CSRF}&start=5&end=1")))
                .unwrap(),
        )
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body_bytes).unwrap();
    assert_eq!(body, "start must be at most end");
}
