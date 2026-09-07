//! App views — request handlers (Django-style "views").
//!
//! Each handler is a stateless async fn; `urls.rs` mounts them
//! under their HTTP paths. For pure-CRUD admin needs you don't
//! need any custom views — `rustango::admin::router(pool)` covers
//! that. Replace the stub below with your own handlers and add
//! corresponding `.route(...)` lines in `urls.rs`.

use axum::response::Html;

/// `GET /<app-prefix>/hello` — placeholder. Wire the actual path
/// in `urls.rs` once you decide on the app's URL prefix.
///
/// The matching `.route(...)` in `urls.rs` ships commented out on
/// purpose, so nothing references this until you wire it.
#[allow(dead_code)]
pub async fn hello() -> Html<&'static str> {
    Html("<h1>hello from your new app</h1>")
}
