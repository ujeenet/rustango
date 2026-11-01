use axum::{extract::Path, http::StatusCode, response::Json};
use rustango::api_errors::ApiError;
use rustango::extractors::Tenant;

use crate::models::{Author, Post};

pub async fn list_articles(mut tenant: Tenant) -> Result<Json<serde_json::Value>, ApiError> {
    let posts: Vec<Post> = Post::objects()
        .order_by(&[("published_at", true)])
        .fetch_on(tenant.conn().await.map_err(db_error)?)
        .await
        .map_err(db_error)?;

    let items: Vec<serde_json::Value> = posts
        .iter()
        .map(|p| {
            serde_json::json!({
                "id":           p.id.get().copied().unwrap_or(0),
                "title":        &p.title,
                "body":         &p.body,
                "author_id":    p.author_id.pk(),
                "published_at": p.published_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({ "count": items.len(), "results": items }),
    ))
}

pub async fn get_article(
    mut tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use rustango::core::Column as _;
    let posts: Vec<Post> = Post::objects()
        .where_(Post::id.eq(id))
        .fetch_on(tenant.conn().await.map_err(db_error)?)
        .await
        .map_err(db_error)?;
    let Some(post) = posts.into_iter().next() else {
        return Err(ApiError::not_found("article not found"));
    };
    Ok(Json(serde_json::json!({
        "id":           post.id.get().copied().unwrap_or(0),
        "title":        &post.title,
        "body":         &post.body,
        "author_id":    post.author_id.pk(),
        "published_at": post.published_at.to_rfc3339(),
    })))
}

pub async fn list_authors(mut tenant: Tenant) -> Result<Json<serde_json::Value>, ApiError> {
    let authors: Vec<Author> = Author::objects()
        .order_by(&[("name", false)])
        .fetch_on(tenant.conn().await.map_err(db_error)?)
        .await
        .map_err(db_error)?;

    let items: Vec<serde_json::Value> = authors
        .iter()
        .map(|a| {
            serde_json::json!({
                "id":   a.id.get().copied().unwrap_or(0),
                "name": &a.name,
                "bio":  &a.bio,
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({ "count": items.len(), "results": items }),
    ))
}

/// A 500 that logs the cause instead of sending it to the client.
fn db_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, "blog_demo", e)
}
