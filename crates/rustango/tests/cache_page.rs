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

// ---------- Paranoid-review fixes ----------

/// Multi-value `Set-Cookie` headers survive the cache round-trip.
/// Pre-fix: HashMap dedup truncated to one cookie.
#[tokio::test]
async fn multi_value_headers_survive_round_trip() {
    // Regression for the JSON header round-trip: a response with two
    // values of the same header must come back with both after a
    // cache HIT. Uses `Link` — a legitimately multi-valued *and*
    // cacheable header. (This test used to use `Set-Cookie`, which is
    // now deliberately never cached — see
    // `response_setting_a_cookie_is_never_cached` (#1251).)
    use axum::http::{HeaderName, HeaderValue};
    let link = HeaderName::from_static("link");
    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/page",
            get(move || {
                let link = link.clone();
                async move {
                    let mut resp = axum::response::Response::new(Body::from("ok"));
                    resp.headers_mut().append(
                        link.clone(),
                        HeaderValue::from_static("</a.css>; rel=preload"),
                    );
                    resp.headers_mut()
                        .append(link, HeaderValue::from_static("</b.js>; rel=preload"));
                    resp
                }
            }),
        )
        .layer(CachePageLayer::new(cache));

    // Warm the cache.
    let _ = app
        .clone()
        .oneshot(Request::builder().uri("/page").body(Body::empty()).unwrap())
        .await
        .unwrap();
    // Hit.
    let r2 = app
        .clone()
        .oneshot(Request::builder().uri("/page").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        r2.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("HIT")
    );
    let links: Vec<String> = r2
        .headers()
        .get_all(HeaderName::from_static("link"))
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    assert_eq!(links.len(), 2, "both Link headers must survive: {links:?}");
    assert!(links.iter().any(|c| c.contains("a.css")));
    assert!(links.iter().any(|c| c.contains("b.js")));
}

/// Content-Type and other handler-set headers survive a HIT.
#[tokio::test]
async fn content_type_round_trips_through_cache() {
    use axum::http::header::CONTENT_TYPE;
    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/api/data",
            get(|| async { ([(CONTENT_TYPE, "application/json")], r#"{"ok":true}"#) }),
        )
        .layer(CachePageLayer::new(cache));

    let _miss = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/data")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let hit = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/data")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        hit.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("HIT")
    );
    assert_eq!(
        hit.headers()
            .get(CONTENT_TYPE)
            .and_then(|h| h.to_str().ok()),
        Some("application/json"),
        "Content-Type must survive cache round-trip"
    );
}

/// Oversized response bodies pass through with `X-Cache-Status: BYPASS`
/// instead of becoming a 500. Pre-fix: turned a successful 200 into 500.
#[tokio::test]
async fn oversize_body_bypasses_cache_not_500() {
    let cache = Arc::new(InMemoryCache::new());
    // 2 MiB body — exceeds the 1 MiB cap.
    let app: Router = Router::new()
        .route(
            "/big",
            get(|| async {
                let big = vec![b'A'; 2 * (1 << 20)];
                ([(header::CONTENT_TYPE, "text/plain")], big)
            }),
        )
        .layer(CachePageLayer::new(cache));

    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/big").body(Body::empty()).unwrap())
        .await
        .unwrap();
    // Should NOT be 500 — handler succeeded.
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "oversize body must not be transformed into 500"
    );
    // But marked BYPASS so observability sees the issue.
    assert_eq!(
        resp.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("BYPASS")
    );
}

/// `Vary` response header is set on cached responses to communicate
/// our partitioning to downstream caches/CDNs.
#[tokio::test]
async fn vary_response_header_is_set_for_partitioned_responses() {
    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route("/p", get(|| async { "ok" }))
        .layer(CachePageLayer::new(cache).vary_on(["accept-language"]));

    // Warm + hit.
    let _miss = app
        .clone()
        .oneshot(Request::builder().uri("/p").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let hit = app
        .clone()
        .oneshot(Request::builder().uri("/p").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let vary: Vec<String> = hit
        .headers()
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    let joined = vary.join(", ");
    assert!(
        joined.to_ascii_lowercase().contains("accept-language"),
        "Vary must include partitioned headers: got {joined:?}"
    );
    assert!(
        joined.to_ascii_lowercase().contains("host"),
        "Vary must include host (default partition): got {joined:?}"
    );
}

/// Different Host headers partition the cache by default — multi-tenant
/// apps don't get cross-tenant hits without explicit configuration.
#[tokio::test]
async fn host_header_partitions_cache_by_default() {
    use std::sync::atomic::AtomicU32;
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/tenant",
            get(|hdr: axum::http::HeaderMap| async move {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let host = hdr
                    .get(header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("?");
                format!("n={n}-host={host}")
            }),
        )
        .layer(CachePageLayer::new(cache));

    let r_a = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/tenant")
                .header(header::HOST, "alpha.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_to_string(r_a.into_body()).await,
        "n=0-host=alpha.example.com"
    );

    let r_b = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/tenant")
                .header(header::HOST, "beta.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_to_string(r_b.into_body()).await,
        "n=1-host=beta.example.com"
    );

    // Repeated alpha → cache hit on alpha's earlier body.
    let r_a2 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/tenant")
                .header(header::HOST, "alpha.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_to_string(r_a2.into_body()).await,
        "n=0-host=alpha.example.com"
    );
    assert_eq!(COUNTER.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------- QUERY (#1111)

use rustango::http_query::QueryRouterExt as _;

/// Build a `QUERY` request to `/page` with `body`.
fn query_req(body: &'static str) -> Request<Body> {
    Request::builder()
        .method(Method::from_bytes(b"QUERY").unwrap())
        .uri("/page")
        .body(Body::from(body))
        .unwrap()
}

fn query_app(counter: &'static AtomicU32, cache: Arc<InMemoryCache>, cache_query: bool) -> Router {
    Router::new()
        .route(
            "/page",
            get(|| async { "list" }).query(move |body: String| async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                format!("q{n}:{body}")
            }),
        )
        .layer(CachePageLayer::new(cache).cache_query(cache_query))
}

#[tokio::test]
async fn query_caching_off_by_default() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    // cache_query defaults to false → QUERY bypasses the cache.
    let app = query_app(&COUNTER, cache, false);

    for _ in 0..2 {
        let resp = app.clone().oneshot(query_req("q=x")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("x-cache-status").is_none());
    }
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        2,
        "both QUERYs reached the handler"
    );
}

#[tokio::test]
async fn query_same_body_hits_cache_when_enabled() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    let app = query_app(&COUNTER, cache, true);

    let r1 = app.clone().oneshot(query_req("q=x")).await.unwrap();
    assert_eq!(
        r1.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("MISS")
    );
    assert_eq!(body_to_string(r1.into_body()).await, "q0:q=x");

    // Byte-identical body → HIT; handler does not run again.
    let r2 = app.clone().oneshot(query_req("q=x")).await.unwrap();
    assert_eq!(
        r2.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("HIT")
    );
    assert_eq!(body_to_string(r2.into_body()).await, "q0:q=x");
    assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn query_different_body_misses() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    let app = query_app(&COUNTER, cache, true);

    let r1 = app.clone().oneshot(query_req("q=x")).await.unwrap();
    assert_eq!(body_to_string(r1.into_body()).await, "q0:q=x");
    // Different body → different cache key → MISS, handler runs again.
    let r2 = app.clone().oneshot(query_req("q=y")).await.unwrap();
    assert_eq!(
        r2.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("MISS")
    );
    assert_eq!(body_to_string(r2.into_body()).await, "q1:q=y");
    assert_eq!(COUNTER.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn query_response_forced_private() {
    // A QUERY handler that (mistakenly) marks its result publicly
    // cacheable must not leak into shared caches — the layer downgrades
    // it to `private` while preserving freshness.
    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/page",
            get(|| async { "list" }).query(|_body: String| async move {
                ([(header::CACHE_CONTROL, "public, max-age=60")], "results")
            }),
        )
        .layer(CachePageLayer::new(cache).cache_query(true));

    let resp = app.clone().oneshot(query_req("q=x")).await.unwrap();
    let cc = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .unwrap()
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(cc.contains("private"), "must be private: {cc}");
    assert!(!cc.contains("public"), "public must be stripped: {cc}");
    assert!(cc.contains("max-age=60"), "freshness preserved: {cc}");
}

#[tokio::test]
async fn query_body_over_cap_is_413() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    let app = query_app(&COUNTER, cache, true);

    // 1 MiB + 1 byte QUERY body exceeds the cacheable cap.
    let big = "a".repeat((1 << 20) + 1);
    let req = Request::builder()
        .method(Method::from_bytes(b"QUERY").unwrap())
        .uri("/page")
        .body(Body::from(big))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        resp.headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        Some("BYPASS")
    );
    // The handler never ran (body rejected before dispatch).
    assert_eq!(COUNTER.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn get_and_query_do_not_collide() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    let app = query_app(&COUNTER, cache, true);

    // GET and QUERY share the path but must not share a cache entry
    // (method is in the key), so the GET handler's "list" is never
    // served to a QUERY and vice versa.
    let g = app
        .clone()
        .oneshot(Request::builder().uri("/page").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(body_to_string(g.into_body()).await, "list");

    let q = app.clone().oneshot(query_req("q=x")).await.unwrap();
    assert_eq!(body_to_string(q.into_body()).await, "q0:q=x");
}

// -------------------------------------------------------------- #1251
// A cached page must never carry one user's identity to another.

/// A 200 that mints a `Set-Cookie` is per-user and must not be cached:
/// the second request runs the handler again (fresh cookie), rather than
/// replaying the first user's cookie from cache.
#[tokio::test]
async fn response_setting_a_cookie_is_never_cached() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/me",
            get(|| async {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header(header::SET_COOKIE, format!("session=user-{n}"))
                    .body(Body::from(format!("hi user-{n}")))
                    .unwrap()
            }),
        )
        .layer(CachePageLayer::new(cache));

    let r1 = app
        .clone()
        .oneshot(Request::builder().uri("/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        r1.headers().get(header::SET_COOKIE).unwrap(),
        "session=user-0"
    );

    // Second request: if the response had been cached, we'd replay
    // `session=user-0`. It must not be — the handler runs again.
    let r2 = app
        .clone()
        .oneshot(Request::builder().uri("/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        r2.headers().get(header::SET_COOKIE).unwrap(),
        "session=user-1",
        "a Set-Cookie response was cached and replayed to the next request",
    );
    assert_eq!(body_to_string(r2.into_body()).await, "hi user-1");
}

/// `Cache-Control: private` opts a response out, like `no-store`.
#[tokio::test]
async fn private_response_is_not_cached() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/priv",
            get(|| async {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CACHE_CONTROL, "private, max-age=60")
                    .body(Body::from(format!("body-{n}")))
                    .unwrap()
            }),
        )
        .layer(CachePageLayer::new(cache));

    for _ in 0..2 {
        app.clone()
            .oneshot(Request::builder().uri("/priv").body(Body::empty()).unwrap())
            .await
            .unwrap();
    }
    // Two distinct handler runs → not cached.
    assert_eq!(COUNTER.load(Ordering::SeqCst), 2);
}

/// A request carrying `Cookie` is treated as per-user by default: it is
/// not served a shared cached body, and its own response is not stored.
#[tokio::test]
async fn request_with_cookie_bypasses_shared_cache_by_default() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/p",
            get(|| async {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                format!("body-{n}")
            }),
        )
        .layer(CachePageLayer::new(cache));

    // Prime the cache with an anonymous request.
    app.clone()
        .oneshot(Request::builder().uri("/p").body(Body::empty()).unwrap())
        .await
        .unwrap();

    // A request with a Cookie must not be served the anonymous cached
    // body — it bypasses to the handler.
    let authed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/p")
                .header(header::COOKIE, "session=abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Handler ran a second time for the cookie'd request.
    assert_eq!(COUNTER.load(Ordering::SeqCst), 2);
    assert_eq!(
        authed
            .headers()
            .get("x-cache-status")
            .and_then(|h| h.to_str().ok()),
        None,
        "a cookie-bearing request should bypass, not report HIT",
    );
}

/// Opt-in restores caching for cookie-bearing requests on a route the
/// app has declared public.
#[tokio::test]
async fn cache_authenticated_opt_in_allows_cookie_requests() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/public",
            get(|| async {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                format!("body-{n}")
            }),
        )
        .layer(CachePageLayer::new(cache).cache_authenticated(true));

    for _ in 0..2 {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/public")
                    .header(header::COOKIE, "a=b")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    // Second served from cache → handler ran once.
    assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
}
