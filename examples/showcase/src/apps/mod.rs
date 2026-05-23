//! Sub-apps. Each mirrors the Django shape (`models.rs`, `urls.rs`,
//! `views.rs`, `admin.rs`, `mod.rs`) and the E2E suite has a matching
//! `e2e/tests/<app>/` folder.
//!
//! Phase 1 (this commit): no apps yet — `api()` returns an empty
//! router with a single smoke route mounted at `/__showcase__/info`.
//! Subsequent phases plug each per-app router in via `.merge(...)`.

use axum::response::Json;
use axum::routing::get;
use axum::Router;

/// Aggregated stateless API router. Each sub-app is invited to merge
/// in once it lands.
#[must_use]
pub fn api() -> Router {
    Router::new().route("/__showcase__/info", get(info))
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
        "apps": [],
    }))
}
