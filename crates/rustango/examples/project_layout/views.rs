//! Project layout — `views.rs` (Django shape).
//!
//! Custom HTTP handlers that complement (or sit alongside) the auto-
//! generated rustango admin. Each handler is a stateless async fn
//! taking the axum extractors it needs; `urls.rs` mounts them under
//! their HTTP paths.
//!
//! For purely-CRUD admin needs, you don't need any custom views —
//! `rustango::admin::router(pool)` is enough. The pattern below is
//! for app-specific endpoints (a published-posts feed, a JSON API,
//! a custom dashboard) that live next to the admin.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::Json;
use rustango::core::Column as _;
use rustango::sql::sqlx::PgPool;
use rustango::sql::Fetcher;

use crate::models::{Post, User};

/// `GET /healthz` — the obligatory liveness probe. No DB access; if
/// this returns 200 the binary is up.
pub async fn healthz() -> &'static str {
    "ok"
}

/// `GET /` — landing page. Renders a tiny HTML shell that links into
/// the auto-admin under `/admin`.
pub async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<title>project_layout demo</title>
<h1>project_layout demo</h1>
<ul>
  <li><a href="/admin">auto-admin</a> — every #[derive(Model)] auto-mounted</li>
  <li><a href="/posts/published">published posts (custom JSON view)</a></li>
  <li><a href="/healthz">healthz</a></li>
</ul>"#,
    )
}

/// `GET /posts/published` — JSON list of published posts with their
/// author username. Pulls models via the typed `objects()` builder.
pub async fn published_posts(
    State(pool): State<PgPool>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let posts: Vec<Post> = Post::objects()
        .where_(Post::published.eq(true))
        .fetch(&pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut out = Vec::with_capacity(posts.len());
    for mut post in posts {
        // Lazy-load the author through the FK wrapper.
        let author: &User = post
            .author
            .get(&pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        out.push(serde_json::json!({
            "id": post.id.get().copied(),
            "title": post.title,
            "author": author.username,
        }));
    }
    Ok(Json(serde_json::json!({ "posts": out })))
}

/// `GET /users/:id` — a single User as JSON. Demonstrates path
/// extractors plus the typed where_ builder.
pub async fn user_detail(
    Path(id): Path<i64>,
    State(pool): State<PgPool>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut rows: Vec<User> = User::objects()
        .where_(User::id.eq(id))
        .fetch(&pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let user = rows.pop().ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({
        "id": user.id.get().copied(),
        "username": user.username,
        "active": user.active,
    })))
}
