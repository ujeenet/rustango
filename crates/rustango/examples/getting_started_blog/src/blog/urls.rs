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

use axum::Router;

#[allow(unused_imports)]
use axum::routing::get;
#[allow(unused_imports)]
use super::views;

pub fn api() -> Router<()> {
    Router::new()
        // .route("/blog/hello", get(views::hello))
}
