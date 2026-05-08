//! Admin URL routing — Django's `urls.py` shape.
//!
//! `router(pool)` and `Builder` build the axum [`Router`] that maps each
//! HTTP path to a handler in [`super::views`]. Mounted via
//! `Router::new().nest("/admin", admin::router(pool))`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::core::SqlValue;
use crate::sql::sqlx::PgPool;
use axum::routing::{get, post};
use axum::Router;

use super::errors::AdminError;
use super::views;

/// Future returned by an [`AdminAction`] handler.
pub type AdminActionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), AdminError>> + Send + 'a>>;

/// Bulk action handler. Receives the model's `&PgPool` (not a tenant
/// connection — the admin runs with the connection the request lives
/// on, so search_path is already correct) and the parsed PK list of
/// the rows the operator selected. Return `Ok(())` on success;
/// `AdminError::Internal(...)` for failure (renders as 500). Built-in
/// `delete_selected` uses this signature.
pub type AdminActionFn =
    Arc<dyn for<'a> Fn(&'a PgPool, &'a [SqlValue]) -> AdminActionFuture<'a> + Send + Sync>;

/// Per-table action registry: model `table` name → action name →
/// handler. The action name must also appear in the model's
/// `admin(actions = "...")` allowlist; the registry just maps the
/// allowlisted names to their callables.
pub(crate) type AdminActionRegistry = HashMap<&'static str, HashMap<&'static str, AdminActionFn>>;

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
    /// Display name shown in the sidebar header. `None` → "rustango admin".
    pub(crate) title: Option<String>,
    /// Optional subtitle shown below the title in the sidebar.
    pub(crate) subtitle: Option<String>,
    /// Per-tenant brand name override. Falls back to `title` when
    /// `None`. Set per-request by the tenancy admin from `Org.brand_name`.
    pub(crate) brand_name: Option<String>,
    /// Per-tenant brand tagline. Falls back to `subtitle` when `None`.
    pub(crate) brand_tagline: Option<String>,
    /// Public URL of the tenant logo (e.g. `/__brand__/{slug}/logo.png`).
    pub(crate) brand_logo_url: Option<String>,
    /// Theme mode — `"light"`, `"dark"`, `"auto"`. `None` → `"auto"`.
    pub(crate) theme_mode: Option<String>,
    /// Pre-built CSS variable assignments derived from the tenant's
    /// `primary_color`. Inlined verbatim into `<style>:root{ ... }`;
    /// the tenancy admin builds it via [`branding::build_brand_css`]
    /// which guarantees the body is safelisted.
    pub(crate) tenant_brand_css: Option<String>,
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
    /// User-registered bulk action handlers (slice 11.0). Keyed by
    /// `<table_name>` then `<action_name>`. The built-in
    /// `delete_selected` is hard-coded in the handler so users don't
    /// need to register it. An action name listed in a model's
    /// `admin(actions = "...")` but NOT in this map AND not the built-in
    /// produces a 500 — same defense as the v0.10.6 unknown-action gate.
    pub(crate) actions: AdminActionRegistry,
    /// Pre-fetched permission codenames for the current user.
    /// `None` = superuser (all operations allowed).
    /// `Some(set)` = the effective codename set; `is_visible`,
    /// `is_read_only`, `can_add`, and `can_delete` consult it.
    pub(crate) user_perms: Option<HashSet<String>>,
    /// v0.27.7 — when true, the admin filters out registry-scoped
    /// models (`#[rustango(scope = "registry")]`, e.g. Org /
    /// Operator) from the sidebar + index. Tenant admins live in
    /// the per-tenant pool and can't show cross-tenant data
    /// without leaking; the registry-only models belong to the
    /// operator console. Set automatically by
    /// `TenantAdminBuilder::build()`. Standalone single-tenant
    /// admins (no tenancy) leave this false and see every model
    /// regardless of scope.
    pub(crate) tenant_mode: bool,
    /// v0.27.8 (#78) — `Some(operator_id)` when the current
    /// session is an operator-impersonation cookie. Drives the
    /// "you are impersonating" banner in admin layouts and
    /// tags audit-log entries. `None` for regular tenant-user
    /// logins.
    pub(crate) impersonated_by: Option<i64>,
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

    /// Mark the current session as an operator impersonation
    /// (v0.27.8 #78). Threads `operator_id` into `chrome_context`
    /// so the admin layout renders an unmissable banner +
    /// "End impersonation" button. Wired by
    /// `TenantAdminBuilder::build()` from the validated session
    /// cookie.
    #[must_use]
    pub fn impersonated_by(mut self, operator_id: i64) -> Self {
        self.config.impersonated_by = Some(operator_id);
        self
    }

    /// Tenant-mode filter (v0.27.7): hides registry-scoped models
    /// (`#[rustango(scope = "registry")]`) from the admin sidebar
    /// and index. Wired automatically by
    /// `TenantAdminBuilder::build()`; standalone admins leave it
    /// false. Pre-fix, registry-only models like `Org` / `Operator`
    /// surfaced inside the tenant admin even though they don't
    /// live in the tenant's storage — clicking through could leak
    /// cross-tenant data via search_path on schema-mode tenants
    /// (the registry's `public.rustango_orgs` would resolve).
    #[must_use]
    pub fn tenant_mode(mut self) -> Self {
        self.config.tenant_mode = true;
        self
    }

    /// Set the admin title shown in the sidebar header.
    /// Defaults to `"rustango admin"` when not set.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.config.title = Some(title.into());
        self
    }

    /// Set the subtitle shown below the title in the sidebar (optional).
    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.config.subtitle = Some(subtitle.into());
        self
    }

    /// Per-tenant brand name (overrides [`Self::title`] for the
    /// sidebar header). Wired by the tenancy admin from
    /// `Org.brand_name` per request.
    #[must_use]
    pub fn brand_name(mut self, name: impl Into<String>) -> Self {
        self.config.brand_name = Some(name.into());
        self
    }

    /// Per-tenant brand tagline. Same fallback semantics as
    /// [`Self::brand_name`] — overrides [`Self::subtitle`] when set.
    #[must_use]
    pub fn brand_tagline(mut self, tagline: impl Into<String>) -> Self {
        self.config.brand_tagline = Some(tagline.into());
        self
    }

    /// Public URL of the tenant logo. Rendered as an `<img>` above
    /// the brand name in the sidebar when present.
    #[must_use]
    pub fn brand_logo_url(mut self, url: impl Into<String>) -> Self {
        self.config.brand_logo_url = Some(url.into());
        self
    }

    /// Theme mode — `"light"`, `"dark"`, or `"auto"`. Sets the
    /// `data-theme` attribute on the rendered `<html>` element.
    #[must_use]
    pub fn theme_mode(mut self, mode: impl Into<String>) -> Self {
        self.config.theme_mode = Some(mode.into());
        self
    }

    /// Pre-built per-tenant CSS variable override block. Inlined
    /// inside `<style>:root{ ... }`. Build it via
    /// `crate::tenancy::branding::build_brand_css(&org)`.
    #[must_use]
    pub fn tenant_brand_css(mut self, css: impl Into<String>) -> Self {
        self.config.tenant_brand_css = Some(css.into());
        self
    }

    /// Restrict visible and writable tables to the authenticated user's
    /// effective permission set. Pass the codenames returned by
    /// `rustango::tenancy::permissions::user_permissions(uid, pool)`.
    ///
    /// * Tables where the user lacks `{table}.view` are hidden from the
    ///   index and return 404 on direct hits.
    /// * Tables where the user lacks `{table}.change` are rendered
    ///   read-only (edit form still renders; save returns 403).
    /// * `{table}.add` gates the create form and create submit.
    /// * `{table}.delete` gates delete submit and `delete_selected`.
    ///
    /// Superusers should NOT call this method — omitting it means `None`
    /// which bypasses all permission checks and allows everything.
    pub fn with_user_perms<I: IntoIterator<Item = String>>(mut self, perms: I) -> Self {
        self.config.user_perms = Some(perms.into_iter().collect());
        self
    }

    /// Register a user-defined bulk action handler.
    ///
    /// `model_table` must match the target Model's `table = "..."`
    /// attribute. `action_name` must also appear in that model's
    /// `admin(actions = "...")` allowlist; the attribute is the
    /// allowlist, this is the executable.
    ///
    /// The handler receives the pool and the parsed PK list of the
    /// selected rows. Use it to implement publish, archive, recompute,
    /// etc. — anything that runs over a batch of rows.
    ///
    /// ```ignore
    /// use rustango::sql::sqlx::PgPool;
    /// use rustango::core::SqlValue;
    /// use rustango::admin::AdminError;
    /// async fn mark_published(pool: &PgPool, pks: &[SqlValue]) -> Result<(), AdminError> {
    ///     // ... custom UPDATE here ...
    ///     Ok(())
    /// }
    /// admin::Builder::new(pool)
    ///     .register_action("post", "mark_published", |pool, pks| {
    ///         Box::pin(mark_published(pool, pks))
    ///     })
    ///     .build();
    /// ```
    pub fn register_action<F>(
        mut self,
        model_table: &'static str,
        action_name: &'static str,
        handler: F,
    ) -> Self
    where
        F: for<'a> Fn(&'a PgPool, &'a [SqlValue]) -> AdminActionFuture<'a> + Send + Sync + 'static,
    {
        self.config
            .actions
            .entry(model_table)
            .or_default()
            .insert(action_name, Arc::new(handler));
        self
    }

    pub fn build(self) -> Router {
        Router::new()
            .route("/", get(views::index))
            .route("/__audit", get(super::audit::audit_log_view))
            .route("/__audit/cleanup", post(super::audit::audit_cleanup_submit))
            .route(
                "/{table}",
                get(views::table_view).post(views::create_submit),
            )
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
        let allowlist_ok = self
            .config
            .allowed_tables
            .as_ref()
            .is_none_or(|allowed| allowed.contains(table));
        if !allowlist_ok {
            return false;
        }
        // When a per-user perm set is present, require `{table}.view`.
        if let Some(perms) = &self.config.user_perms {
            return perms.contains(&format!("{table}.view"));
        }
        true
    }

    /// v0.27.7 — scope filter. Tenant admins (`tenant_mode = true`)
    /// hide registry-only models (`#[rustango(scope = "registry")]`,
    /// e.g. `Org` / `Operator`) so cross-tenant data can't surface
    /// inside a tenant subdomain. Standalone admins return true for
    /// every scope.
    pub(crate) fn scope_visible(&self, scope: crate::core::ModelScope) -> bool {
        if !self.config.tenant_mode {
            return true;
        }
        scope == crate::core::ModelScope::Tenant
    }

    /// Returns `true` when the table's mutating routes (edit/update)
    /// should be blocked. Checks the global/per-table read-only flags
    /// first; when `user_perms` is set also checks `{table}.change`.
    pub(crate) fn is_read_only(&self, table: &str) -> bool {
        if self.config.read_only_all || self.config.read_only_tables.contains(table) {
            return true;
        }
        if let Some(perms) = &self.config.user_perms {
            return !perms.contains(&format!("{table}.change"));
        }
        false
    }

    /// `true` when the user may create rows in `table`.
    pub(crate) fn can_add(&self, table: &str) -> bool {
        if self.config.read_only_all || self.config.read_only_tables.contains(table) {
            return false;
        }
        if let Some(perms) = &self.config.user_perms {
            return perms.contains(&format!("{table}.add"));
        }
        true
    }

    /// `true` when the user may delete rows from `table`.
    pub(crate) fn can_delete(&self, table: &str) -> bool {
        if self.config.read_only_all || self.config.read_only_tables.contains(table) {
            return false;
        }
        if let Some(perms) = &self.config.user_perms {
            return perms.contains(&format!("{table}.delete"));
        }
        true
    }

    /// Look up a registered action handler. Returns `None` for the
    /// built-in `delete_selected` (which the handler short-circuits)
    /// and for action names that haven't been registered.
    pub(crate) fn action_handler(&self, table: &str, action: &str) -> Option<AdminActionFn> {
        self.config
            .actions
            .get(table)
            .and_then(|m| m.get(action))
            .cloned()
    }
}

#[cfg(test)]
mod scope_filter_tests {
    use super::*;
    use crate::core::ModelScope;
    use std::sync::Arc;

    fn state_with(tenant_mode: bool) -> AppState {
        let mut cfg = Config::default();
        cfg.tenant_mode = tenant_mode;
        AppState {
            // sqlx PgPool isn't trivially constructable in unit
            // tests; use a lazy connect to a non-existent URL —
            // none of the methods we exercise here touch the pool.
            pool: PgPool::connect_lazy("postgres://_:_@127.0.0.1:1/_unused")
                .expect("connect_lazy never fails"),
            config: Arc::new(cfg),
        }
    }

    #[tokio::test]
    async fn standalone_admin_sees_every_scope() {
        // v0.27.7 regression guard: single-tenant projects must
        // continue to see registry-scoped models in their admin.
        let state = state_with(false);
        assert!(state.scope_visible(ModelScope::Tenant));
        assert!(state.scope_visible(ModelScope::Registry));
    }

    #[tokio::test]
    async fn tenant_admin_hides_registry_scoped_models() {
        // v0.27.7 fix: tenant admins must NOT surface
        // `#[rustango(scope = "registry")]` models (Org / Operator
        // etc.) — those don't live in the tenant pool and clicking
        // them on a schema-mode tenant would leak cross-tenant
        // data via search_path.
        let state = state_with(true);
        assert!(state.scope_visible(ModelScope::Tenant));
        assert!(!state.scope_visible(ModelScope::Registry));
    }

    #[tokio::test]
    async fn tenant_mode_setter_flips_flag() {
        let pool = PgPool::connect_lazy("postgres://_:_@127.0.0.1:1/_unused")
            .expect("connect_lazy never fails");
        let builder = Builder::new(pool).tenant_mode();
        assert!(builder.config.tenant_mode);
    }
}
