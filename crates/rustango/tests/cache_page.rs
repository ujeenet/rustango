//! Integration tests for `CachePageLayer` + header builders (issue #55).
//! Uses `InMemoryCache` + axum's `oneshot` to exercise the layer
//! end-to-end without a network.

#![cfg(feature = "cache-page")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use http_body_util::BodyExt as _;
use rustango::cache::InMemoryCache;
use rustango::cache_page::{never_cache, vary_on, CacheControl, CachePageLayer};
use tower::ServiceExt as _;

async fn body_to_string(body: Body) -> String {
    let bytes = body.collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn second_get_returns_cached_response() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/page",
            get(|| async {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                format!("body-{n}")
            }),
        )
        .layer(CachePageLayer::new(cache));

    // First request — handler runs.
    let r1 = app
        .clone()
        .oneshot(Request::builder().uri("/page").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        r1.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("MISS")
    );
    assert_eq!(body_to_string(r1.into_body()).await, "body-0");

    // Second request — handler should NOT run; response from cache.
    let r2 = app
        .clone()
        .oneshot(Request::builder().uri("/page").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("HIT")
    );
    // The body matches the FIRST handler output, proving the cache served it.
    assert_eq!(body_to_string(r2.into_body()).await, "body-0");
    // And the handler counter only incremented once.
    assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn post_requests_bypass_the_cache() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/page",
            post(|| async {
                COUNTER.fetch_add(1, Ordering::SeqCst);
                "posted"
            }),
        )
        .layer(CachePageLayer::new(cache));

    // Two POST requests — both should reach the handler.
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/page")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // POSTs bypass — no X-Cache-Status header is set.
        assert!(resp.headers().get("x-cache-status").is_none());
    }
    assert_eq!(COUNTER.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn non_200_responses_are_not_cached() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/oops",
            get(|| async {
                COUNTER.fetch_add(1, Ordering::SeqCst);
                (StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }),
        )
        .layer(CachePageLayer::new(cache));

    // Two 500s — both must run the handler (no caching of errors).
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/oops").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        2,
        "errors must not be cached"
    );
}

#[tokio::test]
async fn no_store_in_response_disables_caching() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/never",
            get(|| async {
                COUNTER.fetch_add(1, Ordering::SeqCst);
                ([(header::CACHE_CONTROL, "no-store")], "fresh")
            }),
        )
        .layer(CachePageLayer::new(cache));

    for _ in 0..2 {
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/never")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        2,
        "Cache-Control: no-store must prevent caching"
    );
}

#[tokio::test]
async fn vary_on_partitions_cache_per_header_value() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/p",
            get(|headers: axum::http::HeaderMap| async move {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let lang = headers
                    .get(header::ACCEPT_LANGUAGE)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("?");
                format!("v={n}-lang={lang}")
            }),
        )
        .layer(CachePageLayer::new(cache).vary_on(["accept-language"]));

    // Two requests with different Accept-Language → both run.
    let r_en = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/p")
                .header(header::ACCEPT_LANGUAGE, "en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_to_string(r_en.into_body()).await, "v=0-lang=en");

    let r_fr = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/p")
                .header(header::ACCEPT_LANGUAGE, "fr")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_to_string(r_fr.into_body()).await, "v=1-lang=fr");

    // Repeating the en request → cache hit for en's previous body.
    let r_en2 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/p")
                .header(header::ACCEPT_LANGUAGE, "en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_to_string(r_en2.into_body()).await, "v=0-lang=en");

    // Handler ran exactly twice (en + fr), not three times.
    assert_eq!(COUNTER.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cache_expires_after_timeout() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/p",
            get(|| async {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                format!("v={n}")
            }),
        )
        .layer(CachePageLayer::new(cache).timeout(Duration::from_millis(50)));

    let r1 = app
        .clone()
        .oneshot(Request::builder().uri("/p").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(body_to_string(r1.into_body()).await, "v=0");

    // Wait past the TTL — InMemoryCache evicts on .get().
    tokio::time::sleep(Duration::from_millis(80)).await;

    let r2 = app
        .clone()
        .oneshot(Request::builder().uri("/p").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(body_to_string(r2.into_body()).await, "v=1");
}

// ---------- Header builders ----------

#[test]
fn cache_control_max_age_public_emits_directive() {
    let v = CacheControl::new().max_age(60).public().build();
    let s = v.to_str().unwrap();
    assert!(s.contains("max-age=60"));
    assert!(s.contains("public"));
}

#[test]
fn cache_control_no_store_renders_no_store() {
    let v = CacheControl::new().no_store().no_cache().build();
    let s = v.to_str().unwrap();
    assert!(s.contains("no-store"));
    assert!(s.contains("no-cache"));
}

#[test]
fn never_cache_directive_is_compatible_with_rfc() {
    let v = never_cache();
    let s = v.to_str().unwrap();
    // Every directive the issue spec lists must be present.
    for needle in ["no-store", "no-cache", "must-revalidate", "max-age=0"] {
        assert!(s.contains(needle), "missing `{needle}` in `{s}`");
    }
}

#[test]
fn vary_on_emits_comma_separated_list() {
    let v = vary_on(["cookie", "user-agent"]);
    assert_eq!(v.to_str().unwrap(), "cookie, user-agent");
}
