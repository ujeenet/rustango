//! Project layout — `urls.rs` (Django shape).
//!
//! Single function `router(pool) -> Router` that wires every HTTP
//! path the project exposes. The auto-admin mounts under `/admin`;
//! custom views from `views.rs` mount alongside. `main.rs` calls
//! this once and binds the result to a TCP listener.

use axum::routing::get;
use axum::Router;
use rustango::admin;
use rustango::sql::sqlx::PgPool;

use crate::views;

/// Build the project's full router.
///
/// Mount layout:
/// * `/`                   → `views::index`
/// * `/healthz`            → `views::healthz`
/// * `/posts/published`    → `views::published_posts`
/// * `/users/{id}`         → `views::user_detail`
/// * `/admin/...`          → `rustango::admin::router(pool)`
pub fn router(pool: PgPool) -> Router {
    let admin = admin::Builder::new(pool.clone()).build();

    // `.with_state(pool)` collapses the typed-state into `()` so we
    // can `.nest("/admin", admin)` — both pieces are then `Router<()>`
    // and compose. The view handlers picked their pool out of state
    // already; admin's stateless from the outside.
    Router::new()
        .route("/", get(views::index))
        .route("/healthz", get(views::healthz))
        .route("/posts/published", get(views::published_posts))
        .route("/users/{id}", get(views::user_detail))
        .with_state(pool)
        .nest("/admin", admin)
}
