//! Project views — request handlers (Django-style "views").

use axum::response::Html;

pub async fn index() -> Html<&'static str> {
    Html(
        "<!doctype html>\n\
         <title>rustango app</title>\n\
         <h1>Hello from Rustango!</h1>\n\
         <p>The auto-admin (if enabled) is at <a href=\"/admin\">/admin</a>.</p>",
    )
}

pub async fn healthz() -> &'static str {
    "ok"
}
