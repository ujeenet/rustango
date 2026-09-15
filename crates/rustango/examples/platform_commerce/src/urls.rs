//! Project URL routing.
//!
//! The commerce app owns `/api/v1/*`, `/shop/*` and `/_soak/*`; this
//! file adds the project-root routes and the admin.

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use rustango::jobs::DatabaseJobQueue;
use rustango::sql::Pool;

use crate::views;

#[must_use]
pub fn api(
    pool: Pool,
    queue: Arc<DatabaseJobQueue>,
    fail_ratio_pct: u8,
    cache: rustango::cache::BoxedCache,
) -> Router<()> {
    Router::new()
        .merge(platform_commerce::commerce::urls::api(
            pool.clone(),
            queue,
            fail_ratio_pct,
            cache,
        ))
        // The auto-admin, mounted under `/__admin` rather than `/admin`
        // so it matches where the SaaS twin's console lands under
        // `RouteConfig::legacy()`. Keeping the two apps' URL maps
        // identical is what lets one set of API tests drive both.
        .nest("/__admin", rustango::admin::router(pool))
        .route("/", get(views::index))
        .route("/healthz", get(views::healthz))
}
