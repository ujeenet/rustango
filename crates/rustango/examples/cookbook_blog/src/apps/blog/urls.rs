//! Tenant-aware HTTP routes for the blog app.
//!
//! `/api/authors` GET / POST exercise the JSON API surface against
//! the per-request tenant pool. See cookbook Chapter 9 for the
//! ViewSet equivalent (which is non-tenant-aware today).

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use axum::Router;

use rustango::core::Op;
use rustango::extractors::Tenant;
use rustango::sql::Auto;

use super::models::Author;

/// Tenant-scoped list + create.
async fn list_or_create(
    mut tenant: Tenant,
) -> Result<Json<Vec<AuthorOut>>, (StatusCode, String)> {
    let rows: Vec<Author> = Author::objects()
        .order_by(&[("id", false)])
        .fetch_on(tenant.conn())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(rows.into_iter().map(AuthorOut::from).collect()))
}

async fn retrieve(
    mut tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Json<AuthorOut>, (StatusCode, String)> {
    let row: Vec<Author> = Author::objects()
        .filter("id", Op::Eq, id)
        .fetch_on(tenant.conn())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    row.into_iter()
        .next()
        .map(|a| Json(AuthorOut::from(a)))
        .ok_or((StatusCode::NOT_FOUND, format!("author {id} not found")))
}

#[derive(serde::Serialize)]
pub struct AuthorOut {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub bio: Option<String>,
}

impl From<Author> for AuthorOut {
    fn from(a: Author) -> Self {
        Self {
            id: match a.id { Auto::Set(v) => v, _ => 0 },
            name: a.name,
            email: a.email,
            bio: a.bio,
        }
    }
}

#[must_use]
pub fn api() -> Router {
    Router::new()
        .route("/api/authors", get(list_or_create))
        .route("/api/authors/{id}", get(retrieve))
}
