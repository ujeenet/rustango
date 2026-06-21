//! Backing test for `docs/middleware.md` — exercises the four middleware
//! patterns the guide documents, all against the real framework API and all
//! DB-free (pure `tower::oneshot`, no pool):
//!
//!   1. locale-aware    — `LocaleMiddleware` + the `ActiveLocale` extractor
//!   2. timezone-aware  — a custom `axum::middleware::from_fn` over the
//!                        `i18n::timezone` task-local
//!   3. security headers — `SecurityHeadersLayer::strict().csp(..)`
//!   4. CSRF            — `csrf::layer()` double-submit-cookie
//!
//! Run: `cargo test -p getting_started_blog --test middleware`

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower::ServiceExt; // .oneshot

/// Read a response body into a `String`.
async fn body_string(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------- 1. locale
// `LocaleMiddleware` resolves a per-request locale (cookie → Accept-Language
// → default) and injects an `ActiveLocale` that any handler can extract.

use rustango::i18n::middleware::{ActiveLocale, LocaleMiddleware};

async fn locale_app() -> Router {
    // First entry is the default; `.default("en")` is explicit here. `ar`
    // (Arabic) is included so we can prove the RTL convenience too.
    let layer = LocaleMiddleware::new(&["en", "fr", "ar"]).default("en");

    Router::new()
        .route(
            "/",
            // The extractor pulls the locale the layer chose for THIS request.
            get(|loc: ActiveLocale| async move { format!("{} {}", loc.0, loc.direction()) }),
        )
        .layer(layer)
}

#[tokio::test]
async fn locale_cookie_wins_over_accept_language() {
    let app = locale_app().await;
    // Cookie says fr, Accept-Language says ar → cookie wins (highest priority).
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::COOKIE, "django_language=fr")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "fr ltr");
}

#[tokio::test]
async fn locale_falls_back_to_accept_language_then_default() {
    // No cookie → negotiate Accept-Language. `ar` is RTL.
    let resp = locale_app()
        .await
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::ACCEPT_LANGUAGE, "ar,en;q=0.5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "ar rtl");

    // Neither cookie nor a supported Accept-Language → the default locale.
    let resp = locale_app()
        .await
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "en ltr");
}

// -------------------------------------------------------------- 2. timezone
// There is no built-in timezone *layer* — instead the framework ships the
// `i18n::timezone` task-local + a header/cookie decoder, and you compose a
// one-line `from_fn` middleware that activates the request's offset. This is
// the canonical "write your own middleware" example in the guide.

use rustango::i18n::timezone::{current_offset, from_request_headers, with_offset};

/// Per-request middleware: decode the client's UTC offset from the
/// `tz_offset` cookie (or a `Time-Zone:` header), then run the rest of the
/// stack with that offset active so `current_offset()` / the `localtime`
/// Tera filter see it. Falls back to UTC when nothing parseable is sent.
async fn timezone_mw(req: Request<Body>, next: Next) -> Response {
    match from_request_headers(req.headers(), "tz_offset") {
        Some(offset) => with_offset(offset, next.run(req)).await,
        None => next.run(req).await,
    }
}

fn timezone_app() -> Router {
    Router::new()
        .route(
            "/now",
            // The handler runs inside the activated scope, so the task-local
            // offset is visible here (and inside any `.await` it makes).
            get(|| async { current_offset().local_minus_utc().to_string() }),
        )
        .layer(from_fn(timezone_mw))
}

#[tokio::test]
async fn timezone_activated_from_cookie_minutes() {
    // `tz_offset=330` = UTC+05:30 (what JS `-getTimezoneOffset()` sends).
    let resp = timezone_app()
        .oneshot(
            Request::builder()
                .uri("/now")
                .header(header::COOKIE, "tz_offset=330")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "19800"); // 330 min * 60 = +19800s
}

#[tokio::test]
async fn timezone_falls_back_to_header_then_utc() {
    // No cookie → the `Time-Zone:` header (signed minutes) is used.
    let resp = timezone_app()
        .oneshot(
            Request::builder()
                .uri("/now")
                .header("Time-Zone", "-300") // UTC-05:00
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "-18000");

    // Nothing sent → UTC (offset 0).
    let resp = timezone_app()
        .oneshot(Request::builder().uri("/now").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "0");
}

// ------------------------------------------------------- 3. security headers
// One `.security_headers(..)` call hardens every response. `strict()` is the
// production preset; `.csp(..)` attaches a Content-Security-Policy.

use rustango::security_headers::{CspBuilder, SecurityHeadersLayer, SecurityHeadersRouterExt};

#[tokio::test]
async fn security_headers_harden_every_response() {
    let app = Router::new()
        .route("/", get(|| async { "ok" }))
        .security_headers(SecurityHeadersLayer::strict().csp(CspBuilder::strict_starter().build()));

    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let h = resp.headers();
    assert!(h.get("strict-transport-security").is_some(), "HSTS");
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(h.get("x-frame-options").unwrap(), "DENY");
    assert!(
        h.get("content-security-policy").is_some(),
        "CSP from the builder"
    );
}

// ------------------------------------------------------------------ 4. CSRF
// Double-submit cookie: a safe GET mints the `rustango_csrf` cookie; an unsafe
// request must echo it back via the `X-CSRF-Token` header (or `_csrf` field).

use rustango::forms::csrf::{self, CSRF_COOKIE};

fn csrf_app() -> Router {
    Router::new()
        .route("/form", get(|| async { "render form" }))
        .route("/submit", post(|| async { "accepted" }))
        .layer(csrf::layer())
}

#[tokio::test]
async fn csrf_get_mints_cookie() {
    let resp = csrf_app()
        .oneshot(Request::builder().uri("/form").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(set.starts_with(CSRF_COOKIE), "mints the CSRF cookie: {set}");
}

#[tokio::test]
async fn csrf_blocks_post_without_token_and_allows_with_it() {
    // POST with no token → 403.
    let resp = csrf_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/submit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // POST that echoes the cookie value in the header → 200 (double-submit).
    let token = "matching-token-value";
    let resp = csrf_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/submit")
                .header(header::COOKIE, format!("{CSRF_COOKIE}={token}"))
                .header("X-CSRF-Token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
