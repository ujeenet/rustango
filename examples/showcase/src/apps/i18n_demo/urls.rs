//! HTTP routes for the i18n demo.
//!
//! * `/i18n/greeting` — single endpoint behind `LocaleMiddleware`,
//!   exercises cookie + Accept-Language resolution.
//! * `/en/i18n/greeting` / `/fr/i18n/greeting` / `/es/i18n/greeting`
//!   — three nested copies of the same handler, each pinned to one
//!   locale via `LocaleMiddleware::new(&[lang]).default(lang)`. This
//!   is the documented Router::nest pattern for URL-prefix locales
//!   that the framework's middleware doc points at (see
//!   `i18n/middleware.rs`).

use axum::response::Json;
use axum::routing::get;
use axum::Router;
use rustango::i18n::middleware::{ActiveLocale, LocaleMiddleware};

#[must_use]
pub fn api() -> Router {
    // Routes exposed at the top level (no URL prefix). LocaleMiddleware
    // picks `ActiveLocale` from cookie / Accept-Language with `en` as
    // the fallback.
    let top = Router::new()
        .route("/i18n/greeting", get(greeting))
        .layer(LocaleMiddleware::new(&["en", "fr", "es"]).default("en"));

    Router::new()
        .merge(top)
        .merge(locale_prefix_router("en"))
        .merge(locale_prefix_router("fr"))
        .merge(locale_prefix_router("es"))
}

/// Build `/<lang>/i18n/greeting` with the locale pinned to `lang` via
/// `LocaleMiddleware::new(&[lang]).default(lang)`. Mirrors Django's
/// `i18n_patterns` semantics: the URL determines the locale.
fn locale_prefix_router(lang: &'static str) -> Router {
    let inner = Router::new()
        .route("/i18n/greeting", get(greeting))
        .layer(LocaleMiddleware::new(&[lang]).default(lang));
    Router::new().nest(&format!("/{lang}"), inner)
}

#[derive(serde::Serialize)]
struct GreetingOut {
    locale: String,
    greeting: &'static str,
}

async fn greeting(active: ActiveLocale) -> Json<GreetingOut> {
    let greeting = match active.0.as_str() {
        "fr" => "Bonjour, monde !",
        "es" => "¡Hola, mundo!",
        _ => "Hello, world!",
    };
    Json(GreetingOut {
        locale: active.0,
        greeting,
    })
}
