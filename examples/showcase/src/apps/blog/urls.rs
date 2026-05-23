//! HTTP routes for the blog app — JSON-only API surface so the
//! playwright suite can drive it with `request.get/post`.
//!
//! The non-tenancy `runserver` attaches the DB handle as an
//! `axum::Extension` whose concrete type depends on the active
//! backend feature: `PgPool` when `postgres` is on, `Pool` otherwise.
//! [`db_pool`] hides that fork so the handlers work the same against
//! every backend.

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use axum::Router;
use rustango::core::Op;
use rustango::sql::{Auto, FetcherPool as _, Pool};

use super::models::Post;

/// Backend-attached pool type. The framework's runserver picks one
/// or the other based on whether `postgres` is in the feature set.
#[cfg(feature = "postgres")]
type AttachedPool = sqlx::PgPool;
#[cfg(not(feature = "postgres"))]
type AttachedPool = Pool;

/// Convert the attached extension into the tri-dialect [`Pool`] enum
/// used by every `_pool` family function.
fn into_pool(p: &AttachedPool) -> Pool {
    #[cfg(feature = "postgres")]
    {
        Pool::from(p.clone())
    }
    #[cfg(not(feature = "postgres"))]
    {
        p.clone()
    }
}

#[must_use]
pub fn api() -> Router {
    Router::new()
        .route("/blog/posts", get(list_posts).post(create_post))
        .route("/blog/posts/{id}", get(retrieve_post))
}

#[derive(serde::Serialize)]
struct PostOut {
    id: i64,
    title: String,
    body: Option<String>,
    published: bool,
    created_at: String,
}

impl From<Post> for PostOut {
    fn from(p: Post) -> Self {
        Self {
            id: match p.id {
                Auto::Set(n) => n,
                Auto::Unset => 0,
            },
            title: p.title,
            body: p.body,
            published: p.published,
            created_at: match p.created_at {
                Auto::Set(t) => t.to_rfc3339(),
                Auto::Unset => String::new(),
            },
        }
    }
}

#[derive(serde::Deserialize)]
struct PostIn {
    title: String,
    body: Option<String>,
    #[serde(default)]
    published: bool,
}

async fn list_posts(
    Extension(pool): Extension<AttachedPool>,
) -> Result<Json<Vec<PostOut>>, (StatusCode, String)> {
    let pool = into_pool(&pool);
    let posts: Vec<Post> = Post::objects()
        .order_by(&[("id", false)]) // ASC — natural list order
        .fetch_pool(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(posts.into_iter().map(PostOut::from).collect()))
}

async fn retrieve_post(
    Extension(pool): Extension<AttachedPool>,
    Path(id): Path<i64>,
) -> Result<Json<PostOut>, (StatusCode, String)> {
    let pool = into_pool(&pool);
    let mut rows: Vec<Post> = Post::objects()
        .filter_op("id", Op::Eq, id)
        .fetch_pool(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(p) = rows.pop() {
        Ok(Json(PostOut::from(p)))
    } else {
        Err((StatusCode::NOT_FOUND, format!("post {id} not found")))
    }
}

async fn create_post(
    Extension(pool): Extension<AttachedPool>,
    Json(input): Json<PostIn>,
) -> Result<(StatusCode, Json<PostOut>), (StatusCode, String)> {
    let pool = into_pool(&pool);
    let mut p = Post {
        id: Auto::Unset,
        title: input.title,
        body: input.body,
        published: input.published,
        created_at: Auto::Unset,
    };
    p.insert_pool(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((StatusCode::CREATED, Json(PostOut::from(p))))
}
