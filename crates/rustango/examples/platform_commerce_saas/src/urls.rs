//! Project URL routing.
//!
//! The commerce app owns `/api/v1/*`, `/shop/*` and `/_soak/*`. There
//! is no admin mount here: under `.tenancy()` the framework mounts the
//! tenant admin and the operator console itself, at whichever paths
//! `RouteConfig` names — `RouteConfig::legacy()` in `main.rs` puts them
//! under `/__`, leaving `/login` and `/admin` to the storefront.

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use crate::commerce::supervisor::QueueMap;
use crate::views;

#[must_use]
pub fn api(queues: Arc<QueueMap>, fail_ratio_pct: u8) -> Router<()> {
    Router::new()
        .merge(crate::commerce::urls::api(queues, fail_ratio_pct))
        .route("/", get(views::index))
        .route("/healthz", get(views::healthz))
}
