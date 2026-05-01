//! Admin URL routing — Django's `urls.py` shape.
//!
//! `router(pool)` and `Builder` build the axum [`Router`] that maps each
//! HTTP path to a handler in [`super::views`]. Mounted via
//! `Router::new().nest("/admin", admin::router(pool))`.

use std::collections::HashSet;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use crate::sql::sqlx::PgPool;

use super::views;

/// Mount the admin under any prefix using axum's nesting:
/// `Router::new().nest("/admin", crate::admin::router(pool))`.
///
/// Equivalent to `Builder::new(pool).build()`. For finer control (model
/// allowlist, read-only tables) use [`Builder`].
pub fn router(pool: PgPool) -> Router {
    Builder::new(pool).build()
}

/// Configurable admin builder.
///
/// ```ignore
/// let app = admin::Builder::new(pool)
///     .show_only(["user", "post", "audit_log"])
///     .read_only(["audit_log"])
///     .build();
/// ```
#[must_use]
pub struct Builder {
    pool: PgPool,
    config: Config,
}

#[derive(Clone, Default)]
pub(crate) struct Config {
    /// Tables visible in the admin. `None` = every registered model.
    pub(crate) allowed_tables: Option<HashSet<String>>,
    /// Tables whose mutating routes are blocked and whose write-buttons
    /// are hidden in HTML.
    pub(crate) read_only_tables: HashSet<String>,
    /// Global read-only mode — when true, **every** visible table is
    /// treated as read-only regardless of `read_only_tables`. Used by
    /// `rustango-tenancy` to gate non-superuser tenant users without
    /// having to enumerate every table at request time.
    pub(crate) read_only_all: bool,
}

impl Builder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            config: Config::default(),
        }
    }

    /// Restrict the admin to these tables. Models not in the list are
    /// hidden from the index and return 404 on direct hits.
    pub fn show_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.allowed_tables = Some(tables.into_iter().map(Into::into).collect());
        self
    }

    /// Mark these tables read-only. List/detail still render; create,
    /// edit, and delete routes return 403, and the corresponding buttons
    /// are hidden in the HTML.
    pub fn read_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config
            .read_only_tables
            .extend(tables.into_iter().map(Into::into));
        self
    }

    /// Mark **every** table read-only — the admin renders list/detail
    /// views but every mutating route returns 403 and write-buttons
    /// are hidden. Used by callers (e.g. `rustango-tenancy` for
    /// non-superuser tenant users) that gate by a runtime flag and
    /// don't want to enumerate every table per request.
    pub fn read_only_all(mut self) -> Self {
        self.config.read_only_all = true;
        self
    }

    pub fn build(self) -> Router {
        Router::new()
            .route("/", get(views::index))
            .route("/{table}", get(views::table_view).post(views::create_submit))
            .route("/{table}/new", get(views::create_form))
            .route("/{table}/__action", post(views::action_submit))
            .route(
                "/{table}/{pk}",
                get(views::detail_view).post(views::update_submit),
            )
            .route("/{table}/{pk}/edit", get(views::edit_form))
            .route("/{table}/{pk}/delete", post(views::delete_submit))
            .with_state(AppState {
                pool: self.pool,
                config: Arc::new(self.config),
            })
    }
}

/// Shared per-request state — the pool plus the resolved `Config`.
/// Cloned on every request (Arc-wrapped Config makes that cheap).
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) pool: PgPool,
    pub(crate) config: Arc<Config>,
}

impl AppState {
    pub(crate) fn is_visible(&self, table: &str) -> bool {
        self.config
            .allowed_tables
            .as_ref()
            .is_none_or(|allowed| allowed.contains(table))
    }

    pub(crate) fn is_read_only(&self, table: &str) -> bool {
        self.config.read_only_all || self.config.read_only_tables.contains(table)
    }
}
