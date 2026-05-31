//! Django-parity #363 — `register_admin_view!` adds a per-model
//! custom URL route to the admin Builder. This verifies the
//! end-to-end wiring: handler reaches HTTP, runs the handler body,
//! returns the response intact.
//!
//! Suffix-collision skip + handler routing both observable from a
//! single test app — one registered view that should mount, one
//! registered view whose suffix collides with `new` (built-in
//! route) that should be silently dropped, and one verb mismatch
//! that should return 405.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "cv_post", display = "title")]
#[allow(dead_code)]
pub struct CvPost {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

// Mounted GET route — should be reachable at /cv_post/duplicate.
rustango::register_admin_view!(
    "cv_post",
    "duplicate",
    Method::GET,
    "Duplicate",
    |_pool, _req| async move {
        use axum::response::{Html, IntoResponse};
        Html("<p>duplicated cv_post</p>").into_response()
    },
);

// Suffix collision — `new` is reserved for the framework's
// create-form route. Registration should be silently skipped at
// build-time (with a tracing warning). The Builder should NOT
// overwrite the built-in handler; GET /cv_post/new should still
// return the framework's create form.
rustango::register_admin_view!(
    "cv_post",
    "new",
    Method::GET,
    "Collides with built-in /new",
    |_pool, _req| async move {
        use axum::response::{Html, IntoResponse};
        Html("<p>this handler should never run</p>").into_response()
    },
);

// POST handler — different verb on the SAME path tests method routing.
rustango::register_admin_view!(
    "cv_post",
    "duplicate",
    Method::POST,
    "Duplicate (POST)",
    |_pool, _req| async move {
        use axum::response::IntoResponse;
        (StatusCode::ACCEPTED, "post-duplicate ran").into_response()
    },
);

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE "cv_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    pool
}

async fn fetch(app: axum::Router, method: Method, uri: &str) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    (status, body)
}

#[tokio::test]
async fn custom_get_view_handler_runs() {
    let pool = fresh_pool().await;
    let (status, body) = fetch(build_app(pool), Method::GET, "/cv_post/duplicate").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("duplicated cv_post"),
        "handler body should be returned, got: {body}"
    );
}

#[tokio::test]
async fn custom_post_view_handler_runs_on_same_path() {
    let pool = fresh_pool().await;
    let (status, body) = fetch(build_app(pool), Method::POST, "/cv_post/duplicate").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.contains("post-duplicate ran"), "got: {body}");
}

#[tokio::test]
async fn reserved_suffix_does_not_overwrite_builtin_new() {
    // Sanity: registering a view at suffix `new` is silently
    // dropped. The framework's built-in `/cv_post/new` (the
    // create form) should still render its own page.
    let pool = fresh_pool().await;
    let (status, body) = fetch(build_app(pool), Method::GET, "/cv_post/new").await;
    assert_eq!(status, StatusCode::OK);
    // The framework's create form contains a form element, NOT
    // the user-supplied marker.
    assert!(
        !body.contains("this handler should never run"),
        "reserved-suffix view leaked into built-in /new route"
    );
    // Sanity — should look like a create form (has the title
    // input or a fieldset). Check for `<form` which the framework
    // always emits.
    assert!(
        body.contains("<form"),
        "built-in /new should still render the framework's create form"
    );
}

#[tokio::test]
async fn unmounted_verb_returns_405() {
    // PATCH was never registered for /duplicate — both registered
    // handlers are GET + POST. axum responds with 405 for an
    // unmatched method on a known path.
    let pool = fresh_pool().await;
    let (status, _) = fetch(build_app(pool), Method::PATCH, "/cv_post/duplicate").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}
