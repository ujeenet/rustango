//! Tiny JSON API showing how to read the tenant user's extras.
//!
//! `GET /users/:username` (signed-in tenant users only) resolves a tenant from the request, fetches
//! the matching `AppUser` via the ORM, and returns the typed extras
//! (`display_name`, `timezone`) inline. The framework's auth path
//! still operates against the core columns of `rustango_users`
//! — extras are pure application data.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use axum::Router;

use rustango::core::Column as _;
use rustango::extractors::{SessionUser, Tenant};

use crate::models::AppUser;

#[derive(serde::Serialize)]
pub struct UserOut {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub timezone: String,
    pub is_superuser: bool,
}

impl From<AppUser> for UserOut {
    fn from(u: AppUser) -> Self {
        Self {
            id: u.id.get().copied().unwrap_or_default(),
            username: u.username,
            display_name: u.display_name,
            timezone: u.timezone,
            is_superuser: u.is_superuser,
        }
    }
}

async fn lookup(
    SessionUser(me): SessionUser,
    mut tenant: Tenant,
    Path(username): Path<String>,
) -> Result<Json<UserOut>, (StatusCode, String)> {
    // A user directory is for signed-in tenant users only.
    if me.is_none() {
        return Err((StatusCode::UNAUTHORIZED, "login required".into()));
    }
    let rows: Vec<AppUser> = AppUser::objects()
        .where_(AppUser::username.eq(username.clone()))
        .fetch_on(tenant.conn().await.map_err(rustango::server_error)?)
        .await
        .map_err(rustango::server_error)?;
    rows.into_iter()
        .next()
        .map(|u| Json(UserOut::from(u)))
        .ok_or((StatusCode::NOT_FOUND, format!("user {username} not found")))
}

async fn healthz() -> &'static str {
    "ok"
}

/// `/healthz` and `/users/{username}`; `main.rs` mounts it at the root.
#[must_use]
pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/users/{username}", get(lookup))
}
