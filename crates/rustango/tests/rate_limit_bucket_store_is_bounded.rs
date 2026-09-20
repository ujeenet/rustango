//! The in-process rate limiter's bucket map is bounded (GHSA-rj6w).
//!
//! Nothing removed entries before this. With `KeyBy::Header` the key is
//! the raw header value — attacker-chosen and uncapped — so one request
//! per random value grew the map until the process died.
//!
//! Two properties, and the second is the one that makes the first safe
//! to have: eviction must not hand anyone a request they had not
//! earned. A bucket that has refilled to capacity is indistinguishable
//! from one that does not exist, so dropping *those* is exact. Dropping
//! a partially-spent bucket would be a bypass, and the test below is
//! what would catch it.

#![cfg(feature = "_axum")]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use tower::ServiceExt as _;

async fn ok() -> &'static str {
    "ok"
}

/// A deliberately tiny ceiling, so the flood below actually reaches
/// the eviction path. With the shipped 100k default a few thousand
/// keys never trigger `make_room` at all, and the guard passes without
/// running the code it is guarding — which is exactly how the first
/// draft of this file passed against a `store.clear()` policy.
const CAP: usize = 64;

fn app(capacity: u32, period: Duration) -> Router {
    Router::new()
        .route("/", get(ok))
        .rate_limit(RateLimitLayer::per_header("x-key", capacity, period).max_buckets(CAP))
}

async fn call(app: &Router, key: &str) -> StatusCode {
    let req = Request::builder()
        .uri("/")
        .header("x-key", key)
        .body(Body::empty())
        .expect("request");
    app.clone().oneshot(req).await.expect("response").status()
}

/// A flood of distinct keys must not grow memory without bound.
///
/// This cannot assert the map's length from outside — the store is
/// private — so it asserts the property that matters and that the old
/// code could not hold: the limiter keeps working, and keeps limiting,
/// after far more distinct keys than any real client population.
///
/// It is here to pin the behaviour eviction must preserve. The bypass
/// case is `a_swept_key_is_not_a_free_pass` below.
#[tokio::test]
async fn a_flood_of_distinct_keys_still_limits() {
    let app = app(2, Duration::from_secs(300));

    for i in 0..(CAP * 20) {
        let k = format!("flood-{i}");
        assert_eq!(call(&app, &k).await, StatusCode::OK, "first use of {k}");
    }

    // A key of our own still gets exactly its capacity, no more.
    assert_eq!(call(&app, "mine").await, StatusCode::OK);
    assert_eq!(call(&app, "mine").await, StatusCode::OK);
    assert_eq!(
        call(&app, "mine").await,
        StatusCode::TOO_MANY_REQUESTS,
        "capacity is 2 — the third request must be refused however many \
         other keys are in flight"
    );
}

/// Eviction must not be a bypass.
///
/// A client that has spent its budget must stay spent. If `make_room`
/// ever dropped a partially-spent bucket — an LRU by insertion order,
/// say, or a blanket `clear()` — the attacker's move would be to flood
/// distinct keys until their own bucket was evicted, then continue with
/// a fresh allowance. That is a worse bug than the leak it would be
/// fixing.
///
/// Sweeping only *full* buckets is what makes this hold: a full bucket
/// and an absent one hand out the same number of requests.
#[tokio::test]
async fn a_swept_key_is_not_a_free_pass() {
    // A long period, so nothing legitimately refills during the test.
    let app = app(2, Duration::from_secs(3600));

    assert_eq!(call(&app, "victim").await, StatusCode::OK);
    assert_eq!(call(&app, "victim").await, StatusCode::OK);
    assert_eq!(call(&app, "victim").await, StatusCode::TOO_MANY_REQUESTS);

    // Now flood, which is the attacker trying to force an eviction.
    for i in 0..(CAP * 20) {
        let _ = call(&app, &format!("evict-{i}")).await;
    }

    assert_eq!(
        call(&app, "victim").await,
        StatusCode::TOO_MANY_REQUESTS,
        "the spent bucket must survive the flood. If this is OK, pressure \
         on the store bought the attacker a fresh allowance and the \
         eviction policy is a rate-limit bypass (GHSA-rj6w)."
    );
}
