use axum::routing::get;
use rustango::server::ApiRouter;

use crate::views::{get_article, list_articles, list_authors};

pub fn api() -> ApiRouter {
    ApiRouter::new()
        .route("/api/articles", get(list_articles))
        .route("/api/articles/{id}", get(get_article))
        .route("/api/authors", get(list_authors))
}
