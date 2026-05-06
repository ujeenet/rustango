//! Tiny JSON API showing how to read the tenant user's extras.
//!
//! `GET /users/:username` resolves a tenant from the request, fetches
//! the matching `AppUser` via the ORM, and returns the typed extras
//! (`display_name`, `timezone`) inline. The framework's auth path
//! still operates against the seven core columns of `rustango_users`
//! — extras are pure application data.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use axum::Router;

use rustango::core::Column as _;
use rustango::extractors::Tenant;

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
    mut tenant: Tenant,
    Path(username): Path<String>,
) -> Result<Json<UserOut>, (StatusCode, String)> {
    let rows: Vec<AppUser> = AppUser::objects()
        .where_(AppUser::username.eq(username.clone()))
        .fetch_on(tenant.conn())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    rows.into_iter()
        .next()
        .map(|u| Json(UserOut::from(u)))
        .ok_or((StatusCode::NOT_FOUND, format!("user {username} not found")))
}

async fn healthz() -> &'static str {
    "ok"
}

/// Wire under `/api`. Mount onto your `Cli::api(...)` router.
#[must_use]
pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/users/{username}", get(lookup))
}
