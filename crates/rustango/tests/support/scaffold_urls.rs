//! The scaffolded `src/urls.rs` functions the docs' `main.rs` examples call,
//! with the template's signatures, so those examples compile in a test.
#![allow(dead_code)]

use axum::Router;
use rustango::admin;
use rustango::config::Settings;
use rustango::session::{tier_from_env, try_load_session_secret_for_tier, SessionSecretError};
use rustango::sql::Pool;

pub fn api() -> Router<()> {
    Router::new()
}

pub fn admin_prefix(settings: &Settings) -> String {
    admin::Builder::settings_prefix(settings)
        .unwrap_or("/admin")
        .to_owned()
}

pub fn admin_router(pool: Pool, settings: &Settings) -> Result<Router, SessionSecretError> {
    let secret = try_load_session_secret_for_tier(
        &tier_from_env(),
        std::path::Path::new("./var/.rustango_session.key"),
    )?;
    Ok(admin::Builder::from_settings(pool, settings)
        .admin_prefix(admin_prefix(settings))
        .with_session_auth(secret)
        .build())
}
