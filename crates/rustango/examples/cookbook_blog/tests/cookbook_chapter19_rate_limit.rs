//! Cookbook Chapter 19 — Rate limiting.
//!
//! `rustango::rate_limit::RateLimitLayer` is a token-bucket middleware:
//! `per_ip`, `per_header`, or `global`. Under the limit it passes the
//! request through and stamps `X-RateLimit-Limit` / `X-RateLimit-Remaining`;
//! over it, it returns `429 Too Many Requests` with `Retry-After` and a
//! JSON body — no handler call. All **in-process, no DB**; driven by
//! `TestClient`.
//!
//! Run: `cargo test --test cookbook_chapter19_rate_limit`

use std::time::Duration;

use axum::routing::get;
use axum::Router;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::test_client::TestClient;

// §19.146 — a global bucket: the first N requests pass, the next is 429.
#[tokio::test]
async fn global_bucket_allows_then_blocks() {
    // capacity 2 per (long) window → 2 pass, 3rd blocked.
    let app: Router = Router::new()
        .route("/ping", get(|| async { "pong" }))
        .rate_limit(RateLimitLayer::global(2, Duration::from_secs(60)));
    let client = TestClient::new(app);

    let r1 = client.get("/ping").send().await;
    assert_eq!(r1.status, 200);
    // Success carries the rate-limit budget headers.
    assert_eq!(r1.header("x-ratelimit-limit"), Some("2"));
    assert_eq!(r1.header("x-ratelimit-remaining"), Some("1"));

    let r2 = client.get("/ping").send().await;
    assert_eq!(r2.status, 200);
    assert_eq!(r2.header("x-ratelimit-remaining"), Some("0"));

    // Bucket empty → 429 with Retry-After, handler never runs.
    let r3 = client.get("/ping").send().await;
    assert_eq!(r3.status, 429);
    assert!(r3.header("retry-after").is_some(), "429 must carry Retry-After");
    assert_eq!(r3.header("x-ratelimit-remaining"), Some("0"));
    assert!(
        r3.text().contains("rate limit exceeded"),
        "JSON error body: {}",
        r3.text()
    );
}

// §19.147 — per-header buckets: different header values are independent,
// so one client exhausting its quota doesn't block another.
#[tokio::test]
async fn per_header_buckets_are_independent() {
    let app: Router = Router::new()
        .route("/api", get(|| async { "ok" }))
        .rate_limit(RateLimitLayer::per_header(
            "x-api-key",
            1,
            Duration::from_secs(60),
        ));
    let client = TestClient::new(app);

    // Key "alice": 1 allowed, 2nd blocked.
    assert_eq!(client.get("/api").header("x-api-key", "alice").send().await.status, 200);
    assert_eq!(client.get("/api").header("x-api-key", "alice").send().await.status, 429);

    // Key "bob" has its own bucket — still allowed.
    assert_eq!(client.get("/api").header("x-api-key", "bob").send().await.status, 200);
}
