use axum::{extract::Path, response::Json};
use rustango::extractors::Tenant;

use crate::models::{Author, Post};

pub async fn list_articles(mut tenant: Tenant) -> Json<serde_json::Value> {
    let posts: Vec<Post> = match Post::objects()
        .order_by(&[("published_at", true)])
        .fetch_on(tenant.conn())
        .await
    {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };

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

    Json(serde_json::json!({ "count": items.len(), "results": items }))
}

pub async fn get_article(mut tenant: Tenant, Path(id): Path<i64>) -> Json<serde_json::Value> {
    use rustango::core::Column as _;
    let posts: Vec<Post> = match Post::objects()
        .where_(Post::id.eq(id))
        .fetch_on(tenant.conn())
        .await
    {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };
    let Some(post) = posts.into_iter().next() else {
        return Json(serde_json::json!({"error": "not found"}));
    };
    Json(serde_json::json!({
        "id":           post.id.get().copied().unwrap_or(0),
        "title":        &post.title,
        "body":         &post.body,
        "author_id":    post.author_id.pk(),
        "published_at": post.published_at.to_rfc3339(),
    }))
}

pub async fn list_authors(mut tenant: Tenant) -> Json<serde_json::Value> {
    let authors: Vec<Author> = match Author::objects()
        .order_by(&[("name", false)])
        .fetch_on(tenant.conn())
        .await
    {
        Ok(a) => a,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };

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

    Json(serde_json::json!({ "count": items.len(), "results": items }))
}
