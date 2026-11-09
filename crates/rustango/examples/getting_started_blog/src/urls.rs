//! Project URL routing (template: fullstack — ORM + admin).
//!
//! `Router::new()` in `api()` is the auto-mount anchor —
//! `manage startapp` inserts `.merge(crate::<name>::urls::api())`
//! lines here.

use axum::routing::get;
use axum::Router;
use rustango::admin;
use rustango::config::Settings;
use rustango::session::{tier_from_env, try_load_session_secret_for_tier, SessionSecretError};
use rustango::sql::Pool;

use crate::views;

pub fn api() -> Router<()> {
    Router::new()
        .merge(crate::blog::urls::api())
        .route("/", get(views::index))
        .route("/healthz", get(views::healthz))
}

/// Everything `main.rs` mounts under `/api`: the posts and the sign-in routes (Step 14).
pub fn rest_api(pool: Pool) -> Router {
    crate::post_view_set::PostViewSet::router("/posts", pool.clone())
        .merge(crate::blog::urls::auth_routes(pool))
}

/// Where the admin mounts: `[admin] url_prefix` (or `[routes] admin_url`), else `/admin`.
pub fn admin_prefix(settings: &Settings) -> String {
    admin::Builder::settings_prefix(settings)
        .unwrap_or("/admin")
        .to_owned()
}

/// The auto-admin, behind a superuser login. Create the first account with
/// `cargo run -- create-admin <username> --superuser`.
///
/// The signing key comes from `RUSTANGO_SESSION_SECRET`, else (off the
/// prod tier) from `./var/`, so a restart keeps you signed in. On prod a
/// missing key is an error `main` prints before it exits.
pub fn admin_router(pool: Pool, settings: &Settings) -> Result<Router, SessionSecretError> {
    let secret = try_load_session_secret_for_tier(
        &tier_from_env(),
        std::path::Path::new("./var/.rustango_session.key"),
    )?;
    Ok(admin::Builder::from_settings(pool, settings)
        .admin_prefix(admin_prefix(settings)) // the path main.rs nests it at
        .with_session_auth(secret)
        .build())
}
