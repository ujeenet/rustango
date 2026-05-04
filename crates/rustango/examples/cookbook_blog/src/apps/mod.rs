//! Sub-apps. Each subdir mirrors the Django shape:
//! `models.rs`, `urls.rs`, `views.rs`, `admin.rs`, `mod.rs`.
//!
//! Slice 1 ships empty stubs so the project compiles end-to-end while
//! later slices populate each app.

pub mod tenants;
pub mod auth;
pub mod blog;
pub mod media;
pub mod notify;
pub mod jobs_demo;
pub mod search;
pub mod admin_ui;

use axum::Router;

/// Aggregated stateless API router. Each sub-app is invited to merge in.
#[must_use]
pub fn api() -> Router {
    Router::new()
        .merge(tenants::urls::api())
        .merge(auth::urls::api())
        .merge(blog::urls::api())
        .merge(media::urls::api())
        .merge(notify::urls::api())
        .merge(jobs_demo::urls::api())
        .merge(search::urls::api())
        .merge(admin_ui::urls::api())
}
