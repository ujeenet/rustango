//! Auto-generated CRUD admin for rustango models.
//!
//! Walks the `inventory` registry every `#[derive(Model)]` populates and
//! serves an axum [`axum::Router`] over it — no per-model code required.
//!
//! ```ignore
//! use rustango::{migrate, admin};
//! use rustango::sql::sqlx::PgPool;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
//!     migrate::apply_all(&pool).await?;
//!
//!     let app = admin::router(pool);
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
//!     axum::serve(listener, app).await?;
//!     Ok(())
//! }
//! ```
//!
//! Routes:
//! * `GET  /`                          — list every registered model
//! * `GET  /<table>`                   — list rows
//! * `GET  /__admin/<table>/new`        — create form
//! * `POST /<table>`                   — submit create
//! * `GET  /<table>/<pk>`              — detail view
//! * `GET  /<table>/<pk>/edit`         — edit form (PK readonly)
//! * `POST /<table>/<pk>`              — submit edit
//! * `POST /<table>/<pk>/delete`       — submit delete
//!
//! ## Crate layout (Django-shape)
//!
//! - [`urls`] — `router(pool)`, `Builder`, `Config`, `AppState` (route table).
//! - [`views`] — one async fn per route; consumes the inventory registry.
//! - [`helpers`] — model lookup, FK joins, render_cell, render_form, pager.
//! - [`templates`] — bundled Tera registry + render entry-point.
//! - [`errors`] — `AdminError` and its `IntoResponse` impl.
//! - `forms`, `render`, `auth` — value parsing, HTML rendering primitives,
//!   HTTP Basic auth middleware.

mod audit;
mod auth;
pub mod computed_fields;
mod date_hierarchy;
mod errors;
mod forms;
mod helpers;
pub mod inlines;
pub mod list_filters;
mod login_view;
mod manage_admin;
pub mod session;
pub mod user;
// `pub(crate)` so the operator console can reuse `render_input` /
// `render_value_for_input` for its `/orgs/{slug}/edit` form. Stays
// non-public outside the crate — the helpers' signatures are still
// internal-only.
pub(crate) mod render;
mod templates;
mod urls;
mod views;

pub use auth::protect_with_basic_auth;
pub use computed_fields::{ComputedField, ComputedFieldRenderFn};
pub use errors::AdminError;
pub use inlines::{
    InlineAdmin, InlineAdminGeneric, InlineApplyOutcome, InlineFormPanel, InlineKind, InlinePanel,
};
pub use manage_admin::create_admin_cmd;
pub use session::{AdminSession, AdminSessionSecret};
pub use urls::{router, AdminActionFn, AdminActionFuture, Builder};
pub use user::AdminUser;
