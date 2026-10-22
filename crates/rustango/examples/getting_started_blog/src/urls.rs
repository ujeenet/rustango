//! Project URL routing (template: fullstack — ORM + auto-admin).
//!
//! `Router::new()` in `api()` is the auto-mount anchor —
//! `manage startapp` inserts `.merge(crate::<name>::urls::api())`
//! lines here. The auto-admin is built separately via
//! `admin_router(pool)` and nested at `/admin` from `main.rs`.

use axum::routing::get;
use axum::Router;
use rustango::admin;
use rustango::session::{load_session_secret_for_tier, tier_from_env};
use rustango::sql::Pool;

use crate::views;

pub fn api() -> Router<()> {
    Router::new()
        .merge(crate::blog::urls::api())
        .route("/", get(views::index))
        .route("/healthz", get(views::healthz))
}

/// The auto-admin, behind a login. Create the first account with
/// `cargo run -- create-admin <username>`.
///
/// Takes the multi-backend pool, so this file names no driver and
/// compiles on all three (#1272).
pub fn admin_router(pool: Pool) -> Router {
    let secret = load_session_secret_for_tier(
        &tier_from_env(),
        std::path::Path::new("./var/.rustango_session.key"),
    );
    admin::Builder::new(pool)
        .title("Myblog Admin")
        .admin_prefix("/admin") // must match the `.nest("/admin", …)` mount path
        .with_session_auth(secret)
        .build()
}
