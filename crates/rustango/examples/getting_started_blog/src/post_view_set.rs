use axum::middleware::from_fn_with_state;
use axum::Router;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::viewset::ViewSet;

use crate::blog::auth::{require_user_for_writes, AuthorFromToken};
use crate::blog::models::Post;
use crate::post_serializer::PostSerializer;

/// The Post API: anyone can read, writes need a Bearer token (Step 14).
pub struct PostViewSet;

impl PostViewSet {
    pub fn router(prefix: &str, pool: impl Into<Pool>) -> Router {
        let pool = pool.into();
        ViewSet::for_model(
            Post::SCHEMA,
            &["id", "title", "body", "author_id", "published_at"],
        )
        .serializer::<PostSerializer>()
        .filter_fields(&["author_id", "status"])
        .search_fields(&["title", "body"])
        .ordering(&[("published_at", true)])
        .page_size(20)
        .filter_backend(AuthorFromToken)
        // Writes pass the framework's gate because the layer below guards them.
        .allow_anonymous()
        .router(prefix, pool.clone())
        .layer(from_fn_with_state(pool, require_user_for_writes))
    }
}
