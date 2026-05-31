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
use crate::sql::Pool;
use axum::routing::{get, post};
use axum::Router;

use super::errors::AdminError;
use super::views;

/// Future returned by an [`AdminAction`] handler.
pub type AdminActionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), AdminError>> + Send + 'a>>;

/// Bulk action handler. Receives the model's [`crate::sql::Pool`]
/// (backend-erasing enum so the handler runs on PG / MySQL / SQLite
/// alike) and the parsed PK list of the rows the operator selected.
/// Return `Ok(())` on success; `AdminError::Internal(...)` for
/// failure (renders as 500). Built-in `delete_selected` uses this
/// signature.
///
/// v0.36 breaking change: closure now takes `&'a Pool` instead of
/// `&'a PgPool`. User-defined custom action handlers need updating:
///
/// ```ignore
/// // Pre-v0.36:
/// register_admin_action!("post", "publish", "Publish selected", |pool, pks| {
///     Box::pin(async move {
///         sqlx::query("UPDATE post SET published_at = NOW() WHERE id = ANY($1)")
///             .bind(pks).execute(pool).await?;
///         Ok(())
///     })
/// });
///
/// // v0.36+: route through the bi-dialect ORM:
/// register_admin_action!("post", "publish", "Publish selected", |pool, pks| {
///     Box::pin(async move {
///         use rustango::sql::{UpdaterPool as _, Pool};
///         Post::objects()
///             .filter_op("id", rustango::core::Op::In, pks.into())
///             .update()
///             .set("published_at", chrono::Utc::now())
///             .execute_pool(pool)
///             .await?;
///         Ok(())
///     })
/// });
/// ```
pub type AdminActionFn = Arc<
    dyn for<'a> Fn(&'a crate::sql::Pool, &'a [SqlValue]) -> AdminActionFuture<'a> + Send + Sync,
>;

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
///
/// v0.36: accepts anything `Into<crate::sql::Pool>` — `PgPool` /
/// `MySqlPool` / `SqlitePool` all convert via the existing `From`
/// impls on `Pool`, so existing PG call sites keep compiling.
pub fn router(pool: impl Into<Pool>) -> Router {
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
    pool: Pool,
    config: Config,
}

#[derive(Clone, Default)]
pub(crate) struct Config {
    /// Display name shown in the sidebar header. `None` → "Rustango Admin".
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
    /// v0.27.9 (#59) — URL prefix the admin Router is mounted
    /// under. Threaded into every template as `{{ admin_prefix }}`
    /// so hrefs / form actions resolve correctly under any
    /// mount path. Defaults to `/__admin` (the convention every
    /// rustango-tenancy deployment uses); users mounting via
    /// `nest("/admin", admin::router(pool))` override via
    /// `Builder::admin_prefix("/admin")`. Empty string means
    /// "the admin router is the root" — supported but uncommon.
    pub(crate) admin_prefix: String,
    /// v0.28.2 (#77) — URL of the self-serve change-password
    /// page. Rendered as a sidebar link when set so users can
    /// find it. The tenant admin Builder pulls this from
    /// `RouteConfig::change_password_url`. Standalone admins
    /// leave it `None` (no auth surface to wire it to).
    pub(crate) change_password_url: Option<String>,
    /// URL suffix the audit-log view is mounted under (sibling
    /// to `admin_prefix`). The cross-row activity feed renders
    /// at `<admin_prefix><audit_url>` and the cleanup form at
    /// `<admin_prefix><audit_url>/cleanup`. Threaded into every
    /// template as `{{ audit_url }}` so the sidebar / audit-log
    /// pager / detail-page "View full history" links resolve
    /// correctly under any configuration. Default: `/__audit`
    /// (matches the v0.28 hardcoded path); the tenancy admin
    /// Builder pulls this from `RouteConfig::audit_url` —
    /// which since v0.29 (#85) defaults to `/audit` (no
    /// underscores) for friendly-URL projects.
    pub(crate) audit_url: String,
    /// v0.30.19 — URL prefix at which the framework serves
    /// embedded static assets (`rustango.png` logo, `icon.png`
    /// favicon). Threaded into chrome context as `{{ static_url }}`
    /// so admin templates can build absolute URLs (e.g. the
    /// favicon `<link rel="icon" href="{{ static_url }}/icon.png">`).
    /// Defaults to `/__static__`; tenancy admin Builder pulls this
    /// from `RouteConfig::static_url` — `/_static` under
    /// friendly RouteConfig, `/__static__` under default.
    pub(crate) static_url: String,
    /// v0.30.9 — tables for which the admin list view skips the
    /// `SELECT COUNT(*)` round-trip and renders a "Page N" pager
    /// (driven by has-next-page detection on the row count) instead
    /// of "Page N of M". Required for tables in the millions of
    /// rows where COUNT(*) takes seconds even with indexes.
    /// Per-request override: `?count=skip` (or `?count=0`) on the
    /// list URL applies the same skip without a code change.
    pub(crate) skip_count_tables: HashSet<String>,
    /// v0.45 (#253) — opt-in session auth. When `Some`, the
    /// Builder installs `/login` + `/logout` routes and gates
    /// every other admin route behind a valid signed-cookie
    /// session. Set via
    /// [`Builder::with_session_auth`]. `None` = legacy behavior
    /// (no auth, or basic-auth via the separate
    /// `protect_with_basic_auth` wrapper).
    pub(crate) session_secret: Option<crate::session::SessionSecret>,
}

impl Builder {
    pub fn new(pool: impl Into<Pool>) -> Self {
        let pool = pool.into();
        let mut config = Config::default();
        // v0.27.9 (#59) — default admin mount prefix matches the
        // convention every rustango-tenancy deployment uses.
        // Users who mount under a different path (e.g.
        // `nest("/admin", admin::router(pool))`) override via
        // `Builder::admin_prefix(...)`.
        config.admin_prefix = "/__admin".to_owned();
        // Default audit suffix matches v0.28 hardcoded path so
        // standalone admins (no RouteConfig) keep their existing
        // bookmarks. Tenancy admins override via
        // `Builder::audit_url(...)` from `RouteConfig::audit_url`.
        config.audit_url = "/__audit".to_owned();
        // v0.30.19 — default static_url matches the framework's
        // legacy hardcoded path. Tenancy admin overrides via
        // `Builder::static_url(...)` from `RouteConfig::static_url`
        // — `/_static` under friendly, `/__static__` under default.
        config.static_url = "/__static__".to_owned();
        Self { pool, config }
    }

    /// v0.36 — construct a Builder from a parsed [`crate::config::Settings`].
    ///
    /// Mirrors `crate::tenancy::operator_console::OpBrand::from_env`:
    /// defaults first, then `Settings.admin` field overrides, then
    /// `Settings.brand` brand-section fallbacks (so a deploy can set
    /// `brand.name = "Acme"` once and the admin picks it up alongside
    /// the operator console), then `Settings.routes.admin_url` for the
    /// mount prefix when `admin.url_prefix` is unset.
    ///
    /// Imperative builder methods (`.title(...)`, `.read_only(...)`,
    /// `.admin_prefix(...)`, etc.) called *after* this still win — the
    /// settings frame is a starting point, not a lock.
    #[cfg(feature = "config")]
    pub fn from_settings(pool: impl Into<Pool>, settings: &crate::config::Settings) -> Self {
        let mut builder = Self::new(pool);

        // 1. Admin section overrides (most specific).
        let admin = &settings.admin;
        if let Some(t) = admin.title.as_deref() {
            builder = builder.title(t);
        } else if let Some(brand_name) = settings.brand.name.as_deref() {
            // Brand-section fallback — keeps operator console + admin
            // chrome in sync without duplicating the value.
            builder = builder.title(brand_name);
        }
        if let Some(s) = admin.subtitle.as_deref() {
            builder = builder.subtitle(s);
        } else if let Some(t) = settings.brand.tagline.as_deref() {
            builder = builder.subtitle(t);
        }
        if let Some(url) = admin.logo_url.as_deref() {
            builder = builder.brand_logo_url(url);
        } else if let Some(url) = settings.brand.logo_url.as_deref() {
            builder = builder.brand_logo_url(url);
        }
        if let Some(mode) = admin
            .theme_mode
            .as_deref()
            .or(settings.brand.theme_mode.as_deref())
        {
            builder = builder.theme_mode(mode);
        }

        // 2. URL prefix — admin.url_prefix wins, then routes.admin_url,
        //    else the default `/__admin` set by `Builder::new`.
        let url_prefix = admin
            .url_prefix
            .as_deref()
            .or(settings.routes.admin_url.as_deref());
        if let Some(prefix) = url_prefix {
            builder = builder.admin_prefix(prefix);
        }
        if let Some(audit_url) = settings.routes.audit_url.as_deref() {
            builder = builder.audit_url(audit_url);
        }
        if let Some(static_url) = settings.routes.static_url.as_deref() {
            builder = builder.static_url(static_url);
        }
        if let Some(change_password_url) = settings.routes.change_password_url.as_deref() {
            builder = builder.change_password_url(change_password_url);
        }

        // 3. Permissions / read-only lists from the section.
        if !admin.allowed_tables.is_empty() {
            builder = builder.show_only(admin.allowed_tables.iter().cloned());
        }
        if !admin.read_only_tables.is_empty() {
            builder = builder.read_only(admin.read_only_tables.iter().cloned());
        }

        builder
    }

    /// URL prefix the admin Router is mounted under (#59,
    /// v0.27.9). Threaded into every template as
    /// `{{ admin_prefix }}` so hrefs / form actions resolve
    /// correctly under any mount path. Default: `/__admin`.
    /// Pass an empty string when the admin is the root router.
    /// Trailing slash is stripped.
    #[must_use]
    pub fn admin_prefix(mut self, prefix: impl Into<String>) -> Self {
        let s: String = prefix.into();
        let trimmed = s.trim_end_matches('/').to_owned();
        self.config.admin_prefix = trimmed;
        self
    }

    /// v0.45 (#253) — opt into signed-cookie session auth. When set,
    /// `.build()` will:
    ///
    /// 1. Mount `/login` (GET form + POST submit) + `/logout` (POST).
    /// 2. Wrap every other admin route in an auth middleware that
    ///    redirects unauthenticated requests to `/login`.
    /// 3. Render the sidebar "Logout" form so the operator can
    ///    sign out.
    ///
    /// Credentials are stored in the `rustango_admin_users` table
    /// ([`crate::admin::AdminUser`]). Bootstrap the table via
    /// [`crate::server::AppBuilder::bootstrap`] or apply a
    /// migration; create your first operator with
    /// `AdminUser::new_with_password(...).insert(pool)`.
    ///
    /// The signing key comes from
    /// [`crate::session::SessionSecret`] — same primitive
    /// `tenancy::session` uses, so a host running both layers can
    /// share one key (cookie names + payload shapes differ so the
    /// two layers never cross-decode).
    ///
    /// ```ignore
    /// use rustango::session::SessionSecret;
    ///
    /// let secret = SessionSecret::from_env_or_random();
    /// let admin = rustango::admin::Builder::new(pool)
    ///     .admin_prefix("")
    ///     .with_session_auth(secret)
    ///     .build();
    /// ```
    #[must_use]
    pub fn with_session_auth(mut self, secret: crate::session::SessionSecret) -> Self {
        self.config.session_secret = Some(secret);
        // #253 slice B — opt-into the standard `/account/password`
        // route so the sidebar's "Change password" link renders.
        // Operators that already set `change_password_url` to a
        // custom path keep theirs untouched.
        if self.config.change_password_url.is_none() {
            self.config.change_password_url = Some("/account/password".to_owned());
        }
        self
    }

    /// URL suffix the audit-log view is mounted at (sibling to
    /// `admin_prefix`). Trailing slash is stripped. Default:
    /// `/__audit`. Tenant admins set this from
    /// [`crate::tenancy::RouteConfig::audit_url`] (which since
    /// v0.29 #85 defaults to `/audit` — no underscores —
    /// for friendly-URL projects).
    #[must_use]
    pub fn audit_url(mut self, url: impl Into<String>) -> Self {
        let s: String = url.into();
        let trimmed = s.trim_end_matches('/').to_owned();
        self.config.audit_url = trimmed;
        self
    }

    /// URL prefix at which the framework serves embedded static
    /// assets (logo + favicon). v0.30.19. Threaded into chrome
    /// context so admin templates resolve the favicon `<link>`
    /// to the actual route. Tenancy admin Builder pulls this
    /// from [`crate::tenancy::RouteConfig::static_url`] —
    /// `/_static` under friendly, `/__static__` under default.
    #[must_use]
    pub fn static_url(mut self, url: impl Into<String>) -> Self {
        let s: String = url.into();
        let trimmed = s.trim_end_matches('/').to_owned();
        self.config.static_url = trimmed;
        self
    }

    /// URL of the self-serve change-password page (#77,
    /// v0.28.2). When set, the admin sidebar renders a
    /// "Change password" link pointing at this URL. The tenant
    /// admin Builder pulls this from
    /// [`crate::tenancy::RouteConfig::change_password_url`].
    #[must_use]
    pub fn change_password_url(mut self, url: impl Into<String>) -> Self {
        self.config.change_password_url = Some(url.into());
        self
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

    /// Skip the admin list view's `SELECT COUNT(*)` round-trip for
    /// these tables. The pager renders "Page N" (with prev/next
    /// driven by has-next-page detection on the row count) instead
    /// of "Page N of M". Required for tables in the millions of
    /// rows where `COUNT(*)` with WHERE filters takes seconds.
    ///
    /// Per-request escape hatch: any list URL accepts
    /// `?count=skip` (or `?count=0`) to apply the same skip without
    /// a code change — useful for ad-hoc operator queries on big
    /// tables that aren't pre-tagged.
    ///
    /// ```ignore
    /// admin::Builder::new(pool)
    ///     .skip_count_for(["audit_log", "events"])
    ///     .build()
    /// ```
    pub fn skip_count_for<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config
            .skip_count_tables
            .extend(tables.into_iter().map(Into::into));
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
    /// Defaults to `"Rustango Admin"` when not set.
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
        F: for<'a> Fn(&'a crate::sql::Pool, &'a [SqlValue]) -> AdminActionFuture<'a>
            + Send
            + Sync
            + 'static,
    {
        self.config
            .actions
            .entry(model_table)
            .or_default()
            .insert(action_name, Arc::new(handler));
        self
    }

    pub fn build(self) -> Router {
        // v0.37 — admin runs on any backend the `Pool` enum carries
        // (Postgres / MySQL / SQLite). The v0.36 boot-time PG guard
        // is gone: every fetch site in `views.rs` + `audit.rs` now
        // goes through the JSON bridge (`select_*_as_json_pool`) +
        // dialect emitters (`audit::*_pool`, `tenancy::permissions::*_pool`).
        let audit_path = self.config.audit_url.clone();
        let audit_cleanup_path = format!("{audit_path}/cleanup");
        let session_secret = self.config.session_secret.clone();
        let admin_prefix = self.config.admin_prefix.clone();
        let state = AppState {
            pool: self.pool,
            config: Arc::new(self.config),
        };

        let protected = Router::new()
            .route("/", get(views::index))
            .route(&audit_path, get(super::audit::audit_log_view))
            .route(
                &audit_cleanup_path,
                post(super::audit::audit_cleanup_submit),
            )
            .route(
                "/{table}",
                get(views::table_view).post(views::create_submit),
            )
            .route("/{table}/new", get(views::create_form))
            .route("/{table}/__action", post(views::action_submit))
            .route("/{table}/__autocomplete", get(views::autocomplete_view))
            .route(
                "/{table}/{pk}",
                get(views::detail_view).post(views::update_submit),
            )
            .route("/{table}/{pk}/edit", get(views::edit_form))
            .route("/{table}/{pk}/delete", post(views::delete_submit))
            .with_state(state.clone());

        // #363 — Django-shape `ModelAdmin.get_urls()` per-model
        // custom views. Walk the inventory registry once, validate
        // each entry against the framework's built-in route shape,
        // and mount the survivors on the same protected router.
        // Routes mount BEFORE `.with_state(state)` so the
        // closure-captured `pool` inside the handler runs with the
        // same backend the admin was built against.
        let protected = mount_custom_views(protected, state.clone());

        // #253 — when session auth is configured, mount the
        // `/login` + `/logout` routes BEFORE applying the auth
        // middleware so they stay reachable while every other
        // route — including the new `/account/password` page
        // (slice B) — requires a valid session.
        if let Some(secret) = session_secret {
            use std::sync::Arc as StdArc;
            let gate = super::login_view::SessionGate {
                secret: StdArc::new(secret),
                login_path: if admin_prefix.is_empty() {
                    "/login".to_owned()
                } else {
                    format!("{admin_prefix}/login")
                },
                // #253 slice C — superuser-only by default. Future
                // permission-system epics will add a builder knob
                // to flip this off and consult `user_perms` instead.
                require_superuser: true,
            };
            // Account routes layer on the *inside* of the auth
            // middleware — they require a valid session, just like
            // the rest of the admin surface.
            let protected = protected
                .merge(super::login_view::protected_router(state.clone()))
                .route_layer(axum::middleware::from_fn_with_state(
                    gate,
                    super::login_view::require_session,
                ));
            return Router::new()
                .merge(super::login_view::public_router(state))
                .merge(protected);
        }

        protected
    }
}

/// Shared per-request state — the pool plus the resolved `Config`.
/// Cloned on every request (Arc-wrapped Config makes that cheap).
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) pool: Pool,
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

    /// `true` when this table was tagged via
    /// [`Builder::skip_count_for`] — the admin list view skips the
    /// `SELECT COUNT(*)` round-trip and renders a no-total pager.
    pub(crate) fn count_skipped_for_table(&self, table: &str) -> bool {
        self.config.skip_count_tables.contains(table)
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

/// Mount every inventory-registered custom admin view on `router`,
/// skipping any registration whose URL suffix collides with the
/// framework's built-in routes or whose model isn't visible to the
/// current admin instance (`show_only` / hidden tables). Issue #363.
///
/// Mounted shape: `/{table}/{suffix}` with the registered
/// `Method`. `axum::routing::on(MethodFilter, …)` lets a single
/// route bind to one verb without colliding with the framework's
/// built-in handlers on the same prefix.
fn mount_custom_views(mut router: Router, state: AppState) -> Router {
    use axum::extract::Request;
    use axum::http::Method;
    use axum::routing::on;
    use axum::routing::MethodFilter;

    for view in inventory::iter::<super::custom_views::AdminCustomView> {
        if super::custom_views::is_reserved(view.suffix) {
            tracing::warn!(
                target: "rustango::admin",
                table = %view.table,
                suffix = %view.suffix,
                "custom admin view suffix collides with a built-in admin route — skipping"
            );
            continue;
        }
        // Skip views whose table isn't currently registered or is
        // hidden via the Builder's `show_only` / read-only knobs —
        // mounting the route on an invisible table would be
        // unreachable anyway and surfaces less surprisingly as
        // "view not loaded".
        if !state.is_visible(view.table) {
            tracing::debug!(
                target: "rustango::admin",
                table = %view.table,
                suffix = %view.suffix,
                "custom admin view registered for a table that is not visible to this admin instance — skipping"
            );
            continue;
        }

        let method_filter = match view.method {
            Method::GET => MethodFilter::GET,
            Method::POST => MethodFilter::POST,
            Method::PUT => MethodFilter::PUT,
            Method::DELETE => MethodFilter::DELETE,
            Method::PATCH => MethodFilter::PATCH,
            ref other => {
                tracing::warn!(
                    target: "rustango::admin",
                    table = %view.table,
                    suffix = %view.suffix,
                    method = ?other,
                    "custom admin view declared with an unsupported HTTP method — defaulting to GET"
                );
                MethodFilter::GET
            }
        };

        let table = view.table;
        let suffix = view.suffix.trim_start_matches('/');
        let path = format!("/{table}/{suffix}");
        // `view.handler` is a plain fn pointer (Copy); no Arc
        // bookkeeping needed.
        let handler = view.handler;
        // Each mounted closure needs its own owned `Pool` clone so
        // the loop can re-iterate; cloning the outer `state.pool`
        // here (not inside the closure) keeps the loop's
        // `state` alive across iterations.
        let pool_for_handler = state.pool.clone();

        let mounted_handler = move |req: Request| {
            let pool = pool_for_handler.clone();
            async move { handler(pool, req).await }
        };

        router = router.route(&path, on(method_filter, mounted_handler));
    }

    router
}

#[cfg(all(test, feature = "postgres"))]
mod scope_filter_tests {
    use super::*;
    use crate::core::ModelScope;
    use sqlx::PgPool;
    use std::sync::Arc;

    fn lazy_pg_pool() -> sqlx::PgPool {
        // sqlx PgPool isn't trivially constructable in unit tests;
        // use a lazy connect to a non-existent URL — none of the
        // methods these tests exercise touch the pool.
        PgPool::connect_lazy("postgres://_:_@127.0.0.1:1/_unused")
            .expect("connect_lazy never fails")
    }

    fn state_with(tenant_mode: bool) -> AppState {
        let mut cfg = Config::default();
        cfg.tenant_mode = tenant_mode;
        AppState {
            pool: Pool::Postgres(lazy_pg_pool()),
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

    // v0.27.9 (#59) — admin_prefix template variable regression
    // guard. Default must be `/__admin` (the convention used by
    // every rustango-tenancy deployment); setter must trim trailing
    // slashes; empty string must be supported for "admin is the
    // root router" mounts.

    #[tokio::test]
    async fn admin_prefix_defaults_to_admin_underscore() {
        let pool = lazy_pg_pool();
        let builder = Builder::new(pool);
        assert_eq!(builder.config.admin_prefix, "/__admin");
    }

    /// `Builder::skip_count_for` accumulates table names; the
    /// `count_skipped_for_table` checker returns true exactly for
    /// the tagged tables. Untagged tables stay on the COUNT path.
    #[tokio::test]
    async fn skip_count_for_marks_tables_and_checker_reads_them() {
        let pool = lazy_pg_pool();
        let b = Builder::new(pool).skip_count_for(["audit_log", "events"]);
        let state = AppState {
            pool: Pool::Postgres(lazy_pg_pool()),
            config: Arc::new(b.config),
        };
        assert!(state.count_skipped_for_table("audit_log"));
        assert!(state.count_skipped_for_table("events"));
        assert!(!state.count_skipped_for_table("post"));
        assert!(!state.count_skipped_for_table(""));
    }

    /// Multiple `.skip_count_for(...)` calls union the table sets
    /// rather than replacing — same shape as `read_only` does.
    #[tokio::test]
    async fn skip_count_for_unions_across_calls() {
        let pool = lazy_pg_pool();
        let b = Builder::new(pool)
            .skip_count_for(["audit_log"])
            .skip_count_for(["events"]);
        let state = AppState {
            pool: Pool::Postgres(lazy_pg_pool()),
            config: Arc::new(b.config),
        };
        assert!(state.count_skipped_for_table("audit_log"));
        assert!(state.count_skipped_for_table("events"));
    }

    #[tokio::test]
    async fn admin_prefix_setter_strips_trailing_slash() {
        let pool = lazy_pg_pool();
        let b = Builder::new(pool).admin_prefix("/admin/");
        assert_eq!(b.config.admin_prefix, "/admin");
    }

    #[tokio::test]
    async fn admin_prefix_supports_empty_for_root_mount() {
        let pool = lazy_pg_pool();
        let b = Builder::new(pool).admin_prefix("");
        assert_eq!(b.config.admin_prefix, "");
    }

    // v0.36 slice 7 — Settings-driven Builder construction. The
    // settings frame should populate every supported knob; per-call
    // imperative overrides after `from_settings` still win.
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_applies_admin_section_overrides() {
        use crate::config::{AdminSettings, Settings};
        let mut settings = Settings::default();
        settings.admin = AdminSettings {
            title: Some("Acme Admin".into()),
            subtitle: Some("Tenants".into()),
            logo_url: Some("/assets/acme.png".into()),
            theme_mode: Some("dark".into()),
            url_prefix: Some("/admin".into()),
            allowed_tables: vec!["post".into(), "author".into()],
            read_only_tables: vec!["audit_log".into()],
            ..Default::default()
        };
        let b = Builder::from_settings(lazy_pg_pool(), &settings);
        assert_eq!(b.config.title.as_deref(), Some("Acme Admin"));
        assert_eq!(b.config.subtitle.as_deref(), Some("Tenants"));
        assert_eq!(b.config.brand_logo_url.as_deref(), Some("/assets/acme.png"));
        assert_eq!(b.config.theme_mode.as_deref(), Some("dark"));
        assert_eq!(b.config.admin_prefix, "/admin");
        let allowed: Vec<String> = b
            .config
            .allowed_tables
            .as_ref()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        assert!(allowed.contains(&"post".to_string()));
        assert!(allowed.contains(&"author".to_string()));
        assert!(b.config.read_only_tables.contains("audit_log"));
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_falls_back_to_brand_section() {
        // When `settings.admin.title` is unset, the brand-section
        // name + tagline + logo_url propagate so deploys can set
        // brand once and the admin picks it up alongside the
        // operator console.
        use crate::config::{BrandSettings, Settings};
        let mut settings = Settings::default();
        settings.brand = BrandSettings {
            name: Some("Acme".into()),
            tagline: Some("Things".into()),
            logo_url: Some("/brand/logo.png".into()),
            theme_mode: Some("light".into()),
            ..Default::default()
        };
        let b = Builder::from_settings(lazy_pg_pool(), &settings);
        assert_eq!(b.config.title.as_deref(), Some("Acme"));
        assert_eq!(b.config.subtitle.as_deref(), Some("Things"));
        assert_eq!(b.config.brand_logo_url.as_deref(), Some("/brand/logo.png"));
        assert_eq!(b.config.theme_mode.as_deref(), Some("light"));
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_admin_url_prefix_wins_over_routes_section() {
        // `admin.url_prefix` is the most-specific knob and should
        // beat the broader `routes.admin_url` when both are set.
        use crate::config::{AdminSettings, RoutesSettings, Settings};
        let mut settings = Settings::default();
        settings.admin = AdminSettings {
            url_prefix: Some("/custom-admin".into()),
            ..Default::default()
        };
        settings.routes = RoutesSettings {
            admin_url: Some("/admin".into()),
            ..Default::default()
        };
        let b = Builder::from_settings(lazy_pg_pool(), &settings);
        assert_eq!(b.config.admin_prefix, "/custom-admin");
    }
}
