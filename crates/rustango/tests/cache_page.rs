//! Integration tests for `CachePageLayer` + header builders (issue #55).
//! Uses `InMemoryCache` + axum's `oneshot` to exercise the layer
//! end-to-end without a network.

#![cfg(feature = "cache-page")]

// Layers here are `tenant_agnostic`: under `tenancy` there is no tenant
// context. Tenant keying is covered in `cache_page_tenant.rs`.

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(
            CachePageLayer::new(cache)
                .tenant_agnostic(true)
                .vary_on(["accept-language"]),
        );

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
        .layer(
            CachePageLayer::new(cache)
                .tenant_agnostic(true)
                .timeout(Duration::from_millis(50)),
        );

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
    // The whole body still reaches the client, not an empty one (#2218).
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 2 * (1 << 20));
    assert!(body.iter().all(|b| *b == b'A'));
}

/// `Vary` response header is set on cached responses to communicate
/// our partitioning to downstream caches/CDNs.
#[tokio::test]
async fn vary_response_header_is_set_for_partitioned_responses() {
    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new().route("/p", get(|| async { "ok" })).layer(
        CachePageLayer::new(cache)
            .tenant_agnostic(true)
            .vary_on(["accept-language"]),
    );

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(
            CachePageLayer::new(cache)
                .tenant_agnostic(true)
                .cache_query(cache_query),
        )
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
        .layer(
            CachePageLayer::new(cache)
                .tenant_agnostic(true)
                .cache_query(true),
        );

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

fn big_query(auth: bool) -> Request<Body> {
    // 1 MiB + 1 byte: over the cacheable cap.
    let mut b = Request::builder()
        .method(Method::from_bytes(b"QUERY").unwrap())
        .uri("/page");
    if auth {
        b = b.header(header::AUTHORIZATION, "Bearer t");
    }
    b.body(Body::from("a".repeat((1 << 20) + 1))).unwrap()
}

#[tokio::test]
async fn query_body_over_cap_runs_uncached() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    let app = query_app(&COUNTER, cache, true);

    for n in 0..2 {
        let resp = app.clone().oneshot(big_query(false)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-cache-status")
                .and_then(|h| h.to_str().ok()),
            Some("BYPASS")
        );
        // The handler got the whole body.
        let body = body_to_string(resp.into_body()).await;
        assert_eq!(body.len(), format!("q{n}:").len() + (1 << 20) + 1);
    }
    assert_eq!(COUNTER.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authenticated_big_query_reaches_the_handler() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    let app = query_app(&COUNTER, cache, true);

    let resp = app.oneshot(big_query(true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-cache-status").is_none());
    assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn query_body_read_error_is_400() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);
    let cache = Arc::new(InMemoryCache::new());
    let app = query_app(&COUNTER, cache, true);

    let broken = futures_util::stream::iter(vec![
        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"q=")),
        Err(std::io::Error::other("reset")),
    ]);
    let req = Request::builder()
        .method(Method::from_bytes(b"QUERY").unwrap())
        .uri("/page")
        .body(Body::from_stream(broken))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

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

/// An `X-Api-Key` request is per-user like a cookie one: it is neither
/// stored nor served from the shared cache.
#[tokio::test]
async fn request_with_api_key_bypasses_shared_cache_by_default() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/k",
            get(|headers: axum::http::HeaderMap| async move {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let who = headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("anon")
                    .to_owned();
                format!("{who}-{n}")
            }),
        )
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));

    let keyed = || {
        Request::builder()
            .uri("/k")
            .header("x-api-key", "live_1")
            .body(Body::empty())
            .unwrap()
    };
    let r1 = app.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(body_to_string(r1.into_body()).await, "live_1-0");
    // No key: must not get the keyed body.
    let anon = app
        .clone()
        .oneshot(Request::builder().uri("/k").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(body_to_string(anon.into_body()).await, "anon-1");
    // A keyed request is not served the anonymous entry either.
    let r2 = app.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(r2.headers().get("x-cache-status"), None);
    assert_eq!(body_to_string(r2.into_body()).await, "live_1-2");
}

/// A header named with `credential_headers` makes a request per-user too.
#[tokio::test]
async fn an_added_credential_header_bypasses_shared_cache() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let cache = Arc::new(InMemoryCache::new());
    let app: Router = Router::new()
        .route(
            "/t",
            get(|headers: axum::http::HeaderMap| async move {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let who = headers
                    .get("x-team-token")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("anon")
                    .to_owned();
                format!("{who}-{n}")
            }),
        )
        .layer(
            CachePageLayer::new(cache)
                .tenant_agnostic(true)
                .credential_headers(["X-Team-Token"]),
        );
    let keyed = || {
        Request::builder()
            .uri("/t")
            .header("x-team-token", "t1")
            .body(Body::empty())
            .unwrap()
    };
    let r1 = app.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(body_to_string(r1.into_body()).await, "t1-0");
    let anon = app
        .clone()
        .oneshot(Request::builder().uri("/t").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(body_to_string(anon.into_body()).await, "anon-1");
    let r2 = app.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(body_to_string(r2.into_body()).await, "t1-2");
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
        .layer(
            CachePageLayer::new(cache)
                .tenant_agnostic(true)
                .cache_authenticated(true),
        );

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

/// Without `tenancy` the default layer caches; there is no tenant to resolve.
#[cfg(not(feature = "tenancy"))]
#[tokio::test]
async fn default_layer_caches_without_tenancy() {
    let hits = Arc::new(AtomicU32::new(0));
    let h = hits.clone();
    let app: Router = Router::new()
        .route(
            "/page",
            get(move || {
                let h = h.clone();
                async move { h.fetch_add(1, Ordering::SeqCst).to_string() }
            }),
        )
        .layer(CachePageLayer::new(Arc::new(InMemoryCache::new())));
    for _ in 0..2 {
        let req = Request::builder().uri("/page").body(Body::empty()).unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

fn get_req(uri: &str, headers: &[(header::HeaderName, &str)]) -> Request<Body> {
    let mut b = Request::builder().uri(uri);
    for (n, v) in headers {
        b = b.header(n, *v);
    }
    b.body(Body::empty()).unwrap()
}

fn cache_status(resp: &axum::response::Response) -> Option<&str> {
    resp.headers()
        .get("x-cache-status")
        .and_then(|h| h.to_str().ok())
}

/// A gzip body is not replayed to a client that sent no `Accept-Encoding` (#2219).
#[cfg(feature = "compression")]
#[tokio::test]
async fn compressed_response_is_not_replayed_to_other_encodings() {
    use rustango::compression::{CompressionLayer, CompressionRouterExt as _};
    let app = |layer: CachePageLayer| -> Router {
        Router::new()
            .route("/c", get(|| async { "x".repeat(4096) }))
            .compression(CompressionLayer::default())
            .layer(layer.tenant_agnostic(true))
    };
    let gzip = [(header::ACCEPT_ENCODING, "gzip")];

    let plain_app = app(CachePageLayer::new(Arc::new(InMemoryCache::new())));
    let gz = plain_app
        .clone()
        .oneshot(get_req("/c", &gzip))
        .await
        .unwrap();
    assert_eq!(gz.headers()[header::CONTENT_ENCODING], "gzip");
    let plain = plain_app.oneshot(get_req("/c", &[])).await.unwrap();
    assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(body_to_string(plain.into_body()).await, "x".repeat(4096));

    // Keyed on Accept-Encoding, each encoding gets its own entry.
    let keyed =
        app(CachePageLayer::new(Arc::new(InMemoryCache::new())).vary_on(["accept-encoding"]));
    keyed.clone().oneshot(get_req("/c", &gzip)).await.unwrap();
    let hit = keyed.clone().oneshot(get_req("/c", &gzip)).await.unwrap();
    assert_eq!(cache_status(&hit), Some("HIT"));
    assert_eq!(hit.headers()[header::CONTENT_ENCODING], "gzip");
    let plain = keyed.oneshot(get_req("/c", &[])).await.unwrap();
    assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
}

/// One language is not replayed to every client under `LocaleMiddleware` (#2219).
#[tokio::test]
async fn localized_response_is_not_replayed_to_other_languages() {
    use rustango::i18n::middleware::{ActiveLocale, LocaleMiddleware};
    let app = |layer: CachePageLayer| -> Router {
        Router::new()
            .route("/l", get(|l: ActiveLocale| async move { l.0 }))
            .layer(LocaleMiddleware::new(&["en", "fr"]).default("en"))
            .layer(layer.tenant_agnostic(true))
    };
    let fr = [(header::ACCEPT_LANGUAGE, "fr")];
    let en = [(header::ACCEPT_LANGUAGE, "en")];

    let unkeyed = app(CachePageLayer::new(Arc::new(InMemoryCache::new())));
    unkeyed.clone().oneshot(get_req("/l", &fr)).await.unwrap();
    let resp = unkeyed.oneshot(get_req("/l", &en)).await.unwrap();
    assert_eq!(body_to_string(resp.into_body()).await, "en");

    // `Vary: Cookie` needs no key: a cookie request skips the cache.
    let keyed =
        app(CachePageLayer::new(Arc::new(InMemoryCache::new())).vary_on(["accept-language"]));
    keyed.clone().oneshot(get_req("/l", &fr)).await.unwrap();
    let hit = keyed.clone().oneshot(get_req("/l", &fr)).await.unwrap();
    assert_eq!(cache_status(&hit), Some("HIT"));
    assert_eq!(body_to_string(hit.into_body()).await, "fr");
    let resp = keyed.oneshot(get_req("/l", &en)).await.unwrap();
    assert_eq!(body_to_string(resp.into_body()).await, "en");
}

/// `Vary: *` is never cached (#2219).
#[tokio::test]
async fn vary_star_is_not_cached() {
    let app: Router = Router::new()
        .route("/s", get(|| async { ([(header::VARY, "*")], "ok") }))
        .layer(CachePageLayer::new(Arc::new(InMemoryCache::new())).tenant_agnostic(true));
    app.clone().oneshot(get_req("/s", &[])).await.unwrap();
    let resp = app.oneshot(get_req("/s", &[])).await.unwrap();
    assert_eq!(cache_status(&resp), Some("BYPASS"));
}

/// One `invalidate` purges every variant of the path: any query, any
/// vary value. Other paths and hosts keep theirs.
#[tokio::test]
async fn invalidate_purges_every_variant_of_the_path() {
    use rustango::cache_page::PageKey;
    let hits = Arc::new(AtomicU32::new(0));
    let h = hits.clone();
    let layer = CachePageLayer::new(Arc::new(InMemoryCache::new()))
        .tenant_agnostic(true)
        .key_prefix("pages")
        .vary_on(["accept-language"]);
    let app: Router = Router::new()
        .route(
            "/p",
            get(move || {
                h.fetch_add(1, Ordering::SeqCst);
                async { "ok" }
            }),
        )
        .route("/q", get(|| async { "q" }))
        .layer(layer.clone());
    let status = |uri: &'static str, host: &'static str, lang: &'static str| {
        let app = app.clone();
        async move {
            let req = Request::get(uri)
                .header(header::HOST, host)
                .header(header::ACCEPT_LANGUAGE, lang)
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            cache_status(&resp).unwrap().to_owned()
        }
    };
    let variants = [
        ("/p?x=1", "a.test", "en"),
        ("/p?utm_source=x", "a.test", "en-US,en;q=0.9"),
        ("/p", "a.test", "fr"),
    ];
    let kept = [("/q", "a.test", "en"), ("/p", "b.test", "en")];
    for (uri, host, lang) in variants.into_iter().chain(kept) {
        assert_eq!(status(uri, host, lang).await, "MISS");
        assert_eq!(status(uri, host, lang).await, "HIT");
    }

    layer
        .invalidate([&PageKey::new("/p", "a.test")])
        .await
        .unwrap();
    for (uri, host, lang) in variants {
        assert_eq!(status(uri, host, lang).await, "MISS", "{uri} {lang}");
        assert_eq!(status(uri, host, lang).await, "HIT", "{uri} {lang}");
    }
    for (uri, host, lang) in kept {
        assert_eq!(status(uri, host, lang).await, "HIT", "{host}{uri}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 7);

    // A second purge works the same.
    layer
        .invalidate([&PageKey::new("/p", "a.test").query("x=1")])
        .await
        .unwrap();
    assert_eq!(status("/p", "a.test", "fr").await, "MISS");
}

/// Two nests of one cached router keep apart, and `invalidate` takes the full path.
#[tokio::test]
async fn nested_routers_do_not_share_pages() {
    use axum::extract::OriginalUri;
    use rustango::cache_page::PageKey;
    let layer = CachePageLayer::new(Arc::new(InMemoryCache::new())).tenant_agnostic(true);
    let inner: Router = Router::new()
        .route(
            "/about",
            get(|OriginalUri(u): OriginalUri| async move { u.path().to_owned() }),
        )
        .layer(layer.clone());
    let app: Router = Router::new().nest("/en", inner.clone()).nest("/fr", inner);
    let fetch = |uri: &'static str| {
        let app = app.clone();
        async move {
            let req = Request::get(uri)
                .header(header::HOST, "a.test")
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let status = cache_status(&resp).map(str::to_owned);
            (status, body_to_string(resp.into_body()).await)
        }
    };
    assert_eq!(fetch("/en/about").await.1, "/en/about");
    assert_eq!(
        fetch("/fr/about").await,
        (Some("MISS".into()), "/fr/about".into())
    );
    assert_eq!(fetch("/en/about").await.0.as_deref(), Some("HIT"));
    layer
        .invalidate([&PageKey::new("/en/about", "a.test")])
        .await
        .unwrap();
    assert_eq!(fetch("/en/about").await.0.as_deref(), Some("MISS"));
    assert_eq!(fetch("/fr/about").await.0.as_deref(), Some("HIT"));
}

/// An `InMemoryCache` that counts the text and bytes calls.
#[derive(Default)]
struct Counting {
    inner: InMemoryCache,
    text_calls: AtomicU32,
    bytes_calls: AtomicU32,
    last_key: std::sync::Mutex<String>,
}

#[async_trait::async_trait]
impl rustango::cache::Cache for Counting {
    async fn get(&self, key: &str) -> Result<Option<String>, rustango::cache::CacheError> {
        self.text_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key).await
    }
    async fn set(
        &self,
        key: &str,
        value: &str,
        ttl: Option<Duration>,
    ) -> Result<(), rustango::cache::CacheError> {
        self.text_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.set(key, value, ttl).await
    }
    async fn get_bytes(
        &self,
        key: &str,
    ) -> Result<Option<axum::body::Bytes>, rustango::cache::CacheError> {
        self.bytes_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get_bytes(key).await
    }
    async fn set_bytes(
        &self,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), rustango::cache::CacheError> {
        self.bytes_calls.fetch_add(1, Ordering::SeqCst);
        key.clone_into(&mut self.last_key.lock().unwrap());
        self.inner.set_bytes(key, value, ttl).await
    }
    async fn delete(&self, key: &str) -> Result<(), rustango::cache::CacheError> {
        self.inner.delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool, rustango::cache::CacheError> {
        self.inner.exists(key).await
    }
    async fn clear(&self) -> Result<(), rustango::cache::CacheError> {
        self.inner.clear().await
    }
}

fn bytes_app(cache: Arc<Counting>, hits: Arc<AtomicU32>) -> Router {
    Router::new()
        .route(
            "/bin",
            get(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        [(header::CONTENT_TYPE, "application/octet-stream")],
                        vec![0xff_u8, 0, 0xfe],
                    )
                }
            }),
        )
        .layer(CachePageLayer::new(cache).tenant_agnostic(true))
}

/// A page is stored and served through the bytes API only, and a
/// non-UTF-8 body comes back byte for byte.
#[tokio::test]
async fn pages_use_the_bytes_api_only() {
    let cache = Arc::new(Counting::default());
    let hits = Arc::new(AtomicU32::new(0));
    let app = bytes_app(cache.clone(), hits.clone());
    for want in ["MISS", "HIT"] {
        let resp = app.clone().oneshot(get_req("/bin", &[])).await.unwrap();
        assert_eq!(cache_status(&resp), Some(want));
        assert_eq!(
            resp.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], &[0xff, 0, 0xfe]);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        cache.text_calls.load(Ordering::SeqCst),
        2,
        "the page's String get/set are only the generation reads"
    );
    assert_eq!(cache.bytes_calls.load(Ordering::SeqCst), 3, "get, set, get");
}

/// An entry in the old JSON format is a miss, then overwritten.
#[tokio::test]
async fn an_old_json_entry_is_a_miss_then_replaced() {
    let cache = Arc::new(Counting::default());
    let hits = Arc::new(AtomicU32::new(0));
    let app = bytes_app(cache.clone(), hits.clone());
    app.clone().oneshot(get_req("/bin", &[])).await.unwrap();
    let key = cache.last_key.lock().unwrap().clone();
    let old = r#"{"status":200,"headers":[["content-type","text/plain"]],"body_b64":"b2xk"}"#;
    rustango::cache::Cache::set(&cache.inner, &key, old, None)
        .await
        .unwrap();

    let resp = app.clone().oneshot(get_req("/bin", &[])).await.unwrap();
    assert_eq!(cache_status(&resp), Some("MISS"));
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], &[0xff, 0, 0xfe]);
    let resp = app.oneshot(get_req("/bin", &[])).await.unwrap();
    assert_eq!(cache_status(&resp), Some("HIT"));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------- CSP nonce

fn csp_nonce_of(resp: &axum::response::Response) -> String {
    let csp = resp.headers()["content-security-policy"].to_str().unwrap();
    let start = csp.find("'nonce-").expect("a nonce source") + "'nonce-".len();
    csp[start..].split('\'').next().unwrap().to_owned()
}

/// GET the page twice; each response's body nonce, header nonce and cache status.
async fn two_nonced_gets(app: Router) -> Vec<(String, String, Option<String>)> {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let resp = app.clone().oneshot(get_req("/n", &[])).await.unwrap();
        let header = csp_nonce_of(&resp);
        let status = cache_status(&resp).map(str::to_owned);
        let body = body_to_string(resp.into_body()).await;
        let body = body.strip_prefix("<script nonce=\"").unwrap();
        seen.push((body[..body.find('"').unwrap()].to_owned(), header, status));
    }
    seen
}

fn nonced_route() -> Router {
    use axum::Extension;
    use rustango::csp_nonce::{Nonce, CSP_NONCE_PLACEHOLDER};
    Router::new().route(
        "/n",
        get(|Extension(n): Extension<Nonce>| async move {
            (
                [(
                    header::CONTENT_SECURITY_POLICY,
                    format!("script-src {CSP_NONCE_PLACEHOLDER}"),
                )],
                format!("<script nonce=\"{}\">go()</script>", n.value()),
            )
        }),
    )
}

/// Cache inside the nonce layer: a hit carries this request's nonce in
/// the body too, so its inline scripts still run.
#[tokio::test]
async fn a_cached_page_inside_the_nonce_layer_gets_the_new_nonce() {
    use rustango::csp_nonce::{CspNonceLayer, CspNonceRouterExt as _};
    let cache = Arc::new(InMemoryCache::new());
    let app = nonced_route()
        .layer(CachePageLayer::new(cache).tenant_agnostic(true))
        .csp_nonce(CspNonceLayer::default());
    let seen = two_nonced_gets(app).await;
    assert_eq!(
        seen[1].2.as_deref(),
        Some("HIT"),
        "the page is still cached"
    );
    for (body, header, _) in &seen {
        assert_eq!(body, header, "body nonce must match the CSP header");
    }
    assert_ne!(seen[0].1, seen[1].1, "each response has its own nonce");
}

/// Cache outside the nonce layer: one nonce must not be replayed to
/// every visitor, so the page is not stored.
#[tokio::test]
async fn a_nonced_page_outside_the_nonce_layer_is_not_cached() {
    use rustango::csp_nonce::{CspNonceLayer, CspNonceRouterExt as _};
    let cache = Arc::new(InMemoryCache::new());
    let app = nonced_route()
        .csp_nonce(CspNonceLayer::default())
        .layer(CachePageLayer::new(cache).tenant_agnostic(true));
    let seen = two_nonced_gets(app).await;
    assert_ne!(seen[0].1, seen[1].1, "a nonce was served twice");
    for (body, header, status) in &seen {
        assert_eq!(body, header);
        assert_ne!(status.as_deref(), Some("HIT"));
    }
}

/// A reflected stand-in token must never come back as a visitor's nonce.
#[tokio::test]
async fn a_reflected_cache_token_never_gets_the_visitors_nonce() {
    use axum::extract::RawQuery;
    use axum::Extension;
    use rustango::csp_nonce::{
        CspNonceLayer, CspNonceRouterExt as _, Nonce, CSP_NONCE_PLACEHOLDER,
    };
    let cache = Arc::new(InMemoryCache::new());
    let app = Router::new()
        .route(
            "/r",
            get(
                |Extension(n): Extension<Nonce>, RawQuery(q): RawQuery| async move {
                    (
                        [(
                            header::CONTENT_SECURITY_POLICY,
                            format!("script-src {CSP_NONCE_PLACEHOLDER}"),
                        )],
                        format!(
                            "<script nonce=\"{}\"></script>{}",
                            n.value(),
                            q.unwrap_or_default()
                        ),
                    )
                },
            ),
        )
        .layer(CachePageLayer::new(cache).tenant_agnostic(true))
        .csp_nonce(CspNonceLayer::default());
    let uri = "/r?__RUSTANGO_CACHED_NONCE__";
    for _ in 0..2 {
        let resp = app.clone().oneshot(get_req(uri, &[])).await.unwrap();
        let nonce = csp_nonce_of(&resp);
        let body = body_to_string(resp.into_body()).await;
        assert_eq!(
            body.matches(&nonce).count(),
            1,
            "reflected text got the nonce: {body}"
        );
        assert!(body.ends_with("__RUSTANGO_CACHED_NONCE__"), "{body}");
    }
}

/// Every value of the request's `vary_on` headers, joined, so the
/// handler's view of a repeated header is the cache's too.
fn echo_vary_app(cache: Arc<InMemoryCache>, name: &'static str) -> Router {
    Router::new()
        .route(
            "/v",
            get(move |headers: axum::http::HeaderMap| async move {
                let all: Vec<String> = headers
                    .get_all(name)
                    .iter()
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                    .collect();
                format!("seen={}", all.join("|"))
            }),
        )
        .layer(
            CachePageLayer::new(cache)
                .tenant_agnostic(true)
                .cache_authenticated(true)
                .vary_on([name]),
        )
}

/// HTTP/2 sends each cookie in its own field: two users sharing the
/// first cookie must not share a page.
#[tokio::test]
async fn vary_on_keys_every_value_of_a_repeated_header() {
    let app = echo_vary_app(Arc::new(InMemoryCache::new()), "cookie");
    let send = |second: &'static str| {
        let req = Request::builder()
            .uri("/v")
            .header(header::COOKIE, "consent=yes")
            .header(header::COOKIE, second)
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req)
    };
    let alice = send("session=alice").await.unwrap();
    assert_eq!(
        body_to_string(alice.into_body()).await,
        "seen=consent=yes|session=alice"
    );
    let bob = send("session=bob").await.unwrap();
    assert_eq!(
        body_to_string(bob.into_body()).await,
        "seen=consent=yes|session=bob",
        "bob got alice's page"
    );
    // The same pair still hits.
    let again = send("session=alice").await.unwrap();
    assert_eq!(
        again.headers().get("x-cache-status").unwrap(),
        "HIT",
        "a repeated header no longer caches"
    );
}

/// A value that is not UTF-8 is not the same as no header at all.
#[tokio::test]
async fn a_non_utf8_vary_value_does_not_fill_the_missing_entry() {
    let app = echo_vary_app(Arc::new(InMemoryCache::new()), "accept-language");
    let odd = Request::builder()
        .uri("/v")
        .header(
            header::ACCEPT_LANGUAGE,
            axum::http::HeaderValue::from_bytes(b"\xffen").unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let r = app.clone().oneshot(odd).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let plain = Request::builder().uri("/v").body(Body::empty()).unwrap();
    let r = app.clone().oneshot(plain).await.unwrap();
    assert_eq!(
        body_to_string(r.into_body()).await,
        "seen=",
        "the header-less request got the non-UTF-8 page"
    );
}

/// The purge counter outlives a backend's short default TTL, so a purged
/// variant does not come back when that default runs out.
#[tokio::test]
async fn a_short_default_ttl_does_not_resurrect_purged_pages() {
    use rustango::cache_page::PageKey;
    let hits = Arc::new(AtomicU32::new(0));
    let h = hits.clone();
    let cache = Arc::new(InMemoryCache::with_default_ttl(Duration::from_millis(200)));
    let layer = CachePageLayer::new(cache)
        .tenant_agnostic(true)
        .timeout(Duration::from_secs(60));
    let app: Router = Router::new()
        .route(
            "/p",
            get(move || {
                let n = h.fetch_add(1, Ordering::SeqCst);
                async move { format!("v{n}") }
            }),
        )
        .layer(layer.clone());
    let fetch = || {
        let app = app.clone();
        async move {
            let req = Request::get("/p?x=1")
                .header(header::HOST, "a.test")
                .body(Body::empty())
                .unwrap();
            body_to_string(app.oneshot(req).await.unwrap().into_body()).await
        }
    };
    assert_eq!(fetch().await, "v0");
    layer
        .invalidate([&PageKey::new("/p", "a.test")])
        .await
        .unwrap();
    assert_eq!(fetch().await, "v1");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fetch().await, "v1", "the purged v0 page came back");
}

/// `query_keys`: only the named params key the page, in any order; the
/// rest cannot mint new entries. `ignore_query` keys on the path alone.
#[tokio::test]
async fn query_keys_bound_the_entries_a_query_can_create() {
    static RUNS: AtomicU32 = AtomicU32::new(0);
    let page = |layer: CachePageLayer| {
        Router::new()
            .route(
                "/page",
                get(|| async { format!("run-{}", RUNS.fetch_add(1, Ordering::SeqCst)) }),
            )
            .layer(layer.tenant_agnostic(true))
    };
    let status = |app: Router, uri: &'static str| async move {
        let r = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        r.headers()["x-cache-status"].to_str().unwrap().to_owned()
    };

    let named = page(CachePageLayer::new(Arc::new(InMemoryCache::new())).query_keys(["q", "page"]));
    assert_eq!(status(named.clone(), "/page?q=a&page=2").await, "MISS");
    assert_eq!(
        status(named.clone(), "/page?page=2&q=a").await,
        "HIT",
        "order"
    );
    assert_eq!(
        status(named.clone(), "/page?page=2&q=a&x=junk").await,
        "HIT",
        "unnamed"
    );
    assert_eq!(
        status(named.clone(), "/page?q=b&page=2").await,
        "MISS",
        "named"
    );

    let ignored = page(CachePageLayer::new(Arc::new(InMemoryCache::new())).ignore_query());
    assert_eq!(status(ignored.clone(), "/page?x=1").await, "MISS");
    assert_eq!(status(ignored.clone(), "/page?x=2").await, "HIT");

    let raw = page(CachePageLayer::new(Arc::new(InMemoryCache::new())));
    assert_eq!(status(raw.clone(), "/page?x=1").await, "MISS");
    assert_eq!(
        status(raw, "/page?x=2").await,
        "MISS",
        "default keys the raw query"
    );
}
