//! Sub-apps. Each mirrors the Django shape (`models.rs`, `urls.rs`,
//! `views.rs`, `admin.rs`, `mod.rs`) and the E2E suite has a matching
//! `e2e/tests/<app>/` folder.

pub mod blog;
pub mod shop;

use axum::response::Json;
use axum::routing::get;
use axum::Router;

/// Aggregated stateless API router. Each sub-app merges its routes
/// in here; the per-app playwright folder under `e2e/tests/<app>/`
/// exercises them end-to-end.
#[must_use]
pub fn api() -> Router {
    Router::new()
        .route("/__showcase__/info", get(info))
        .merge(blog::urls::api())
        .merge(shop::urls::api())
}

/// Smoke endpoint — used by the E2E playwright suite's readiness
/// probe + by the matrix job to assert which backend it ran against.
async fn info() -> Json<serde_json::Value> {
    let backend = match std::env::var("DATABASE_URL")
        .unwrap_or_default()
        .as_str()
    {
        u if u.starts_with("postgres://") || u.starts_with("postgresql://") => "postgres",
        u if u.starts_with("mysql://") || u.starts_with("mariadb://") => "mysql",
        u if u.starts_with("sqlite:") => "sqlite",
        _ => "unknown",
    };
    Json(serde_json::json!({
        "framework": "rustango",
        "version": env!("CARGO_PKG_VERSION"),
        "backend": backend,
        "apps": ["blog", "shop"],
    }))
}
