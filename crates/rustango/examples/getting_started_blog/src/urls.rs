//! Project URL routing (template: fullstack — ORM + auto-admin).
//!
//! `Router::new()` in `api()` is the auto-mount anchor —
//! `manage startapp` inserts `.merge(crate::<name>::urls::api())`
//! lines here. The auto-admin is built separately via
//! `admin_router(pool)` and nested at `/admin` from `main.rs`.

use axum::routing::get;
use axum::Router;
use rustango::admin;
use rustango::sql::sqlx::PgPool;

use crate::views;

pub fn api() -> Router<()> {
    Router::new()
        .merge(crate::blog::urls::api())
        .route("/", get(views::index))
        .route("/healthz", get(views::healthz))
}

pub fn admin_router(pool: PgPool) -> Router {
    admin::Builder::new(pool)
        .title("Myblog Admin")
        .admin_prefix("/admin") // must match the `.nest("/admin", …)` mount path
        .build()
}
