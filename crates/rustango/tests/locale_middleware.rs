//! Django-parity #406 — `LocaleMiddleware` end-to-end through a real
//! axum router. Verifies the tower layer injects `ActiveLocale` into
//! request extensions and respects cookie / Accept-Language fallback
//! order, plus the documented `Router::nest("/<lang>", ...)` pattern
//! for per-URL-prefix locales (#424 parity by composition).

#![cfg(feature = "sqlite")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use rustango::i18n::middleware::{ActiveLocale, LocaleMiddleware};
use tower::ServiceExt;

async fn echo_locale(active: ActiveLocale) -> String {
    active.0
}

#[tokio::test]
async fn picks_default_when_no_header_or_cookie() {
    let app = Router::new()
        .route("/", get(echo_locale))
        .layer(LocaleMiddleware::new(&["en", "fr"]).default("en"));
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(std::str::from_utf8(&body).unwrap(), "en");
}

#[tokio::test]
async fn picks_from_accept_language_header() {
    let app = Router::new()
        .route("/", get(echo_locale))
        .layer(LocaleMiddleware::new(&["en", "fr", "es"]).default("en"));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("accept-language", "fr-FR,fr;q=0.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(std::str::from_utf8(&body).unwrap(), "fr");
}

#[tokio::test]
async fn cookie_overrides_accept_language() {
    let app = Router::new()
        .route("/", get(echo_locale))
        .layer(LocaleMiddleware::new(&["en", "fr"]).default("en"));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("accept-language", "en")
                .header("cookie", "django_language=fr")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(std::str::from_utf8(&body).unwrap(), "fr");
}

#[tokio::test]
async fn unknown_cookie_value_falls_back_to_accept_language() {
    let app = Router::new()
        .route("/", get(echo_locale))
        .layer(LocaleMiddleware::new(&["en", "fr"]).default("en"));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("accept-language", "fr")
                .header("cookie", "django_language=ja")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(std::str::from_utf8(&body).unwrap(), "fr");
}

/// URL-prefix locale (`/en/foo` / `/fr/foo`) — the documented
/// composition pattern with `Router::nest`. This is the axum-shape
/// answer to Django's `i18n_patterns()` / issue #424.
#[tokio::test]
async fn router_nest_per_locale_pattern() {
    fn locale_router(lang: &'static str) -> Router {
        Router::new()
            .route("/posts", get(echo_locale))
            .layer(LocaleMiddleware::new(&[lang]).default(lang))
    }

    let app = Router::new()
        .nest("/en", locale_router("en"))
        .nest("/fr", locale_router("fr"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/fr/posts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(std::str::from_utf8(&body).unwrap(), "fr");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/en/posts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(std::str::from_utf8(&body).unwrap(), "en");
}
