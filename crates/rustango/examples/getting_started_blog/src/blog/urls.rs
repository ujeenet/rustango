//! App URL routing.
//!
//! `pub fn api() -> Router<()>` — every route this app exposes.
//! The project-root `src/urls.rs` aggregator calls
//! `.merge(crate::<this_app>::urls::api())` so these routes show up
//! at the project's root. Handlers can take
//! `rustango::extractors::Tenant` (in tenancy projects) or extract
//! state via axum's normal `State<...>` mechanism.
//!
//! Starts empty — uncomment the example or add your own routes.
//! Defining `/` or `/healthz` here would clash with the project-
//! root router, so prefer an app-specific prefix like `/blog/...`.

use std::time::Duration;

use axum::routing::post;
use axum::Router;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::sql::Pool;

#[allow(unused_imports)]
use axum::routing::get;
#[allow(unused_imports)]
use super::views;

use super::auth;

pub fn api() -> Router<()> {
    Router::new()
        // .route("/blog/hello", get(views::hello))
}

/// `/register` and `/login` (Step 14). They need the pool, so `main.rs` mounts them.
pub fn auth_routes(pool: Pool) -> Router {
    // Each sign-up costs a password hash: 10 a minute per client IP.
    let register = Router::new()
        .route("/register", post(auth::register))
        .rate_limit(RateLimitLayer::per_ip(10, Duration::from_secs(60)));
    Router::new()
        .route("/login", post(auth::login)) // throttled inside the handler
        .merge(register)
        .with_state(pool)
}
