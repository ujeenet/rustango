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

/// Future returned by an [`AdminActionFn`] handler.
pub type AdminActionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), AdminError>> + Send + 'a>>;

/// Bulk action handler. It receives the [`crate::sql::Pool`] enum, so
/// one handler runs on any backend, plus the parsed PK list of the
/// selected rows. Return `Ok(())`, or `AdminError::Internal(...)` to
/// fail. The built-in `delete_selected` uses this signature.
///
/// The handler takes `&Pool`, not a backend-specific pool. The
/// example shows the older raw-SQL shape and the ORM shape that
/// replaced it:
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

/// Action registry: table name, then action name, then handler. The
/// action name **must** also be in the model's
/// `admin(actions = "...")` allowlist. That attribute is the
/// allowlist; this registry only holds the callables.
pub(crate) type AdminActionRegistry = HashMap<&'static str, HashMap<&'static str, AdminActionFn>>;

/// Mount the admin under any prefix using axum's nesting:
/// `Router::new().nest("/admin", crate::admin::router(pool))`.
///
/// Same as `Builder::new(pool).build()`. Use [`Builder`] for a model
/// allowlist, read-only tables and the other knobs.
///
/// Takes anything that converts into [`Pool`], so a `PgPool`,
/// `MySqlPool` or `SqlitePool` all work.
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
    /// Sidebar header name. `None` means "Rustango Admin".
    pub(crate) title: Option<String>,
    /// Optional subtitle shown below the title in the sidebar.
    pub(crate) subtitle: Option<String>,
    /// Per-tenant brand name. Falls back to `title`. The tenancy
    /// admin sets it per request from `Org.brand_name`.
    pub(crate) brand_name: Option<String>,
    /// Per-tenant brand tagline. Falls back to `subtitle`.
    pub(crate) brand_tagline: Option<String>,
    /// Public URL of the tenant logo, e.g. `/__brand__/{slug}/logo.png`.
    pub(crate) brand_logo_url: Option<String>,
    /// `"light"`, `"dark"` or `"auto"`. `None` means `"auto"`.
    pub(crate) theme_mode: Option<String>,
    /// CSS variable assignments built from the tenant's
    /// `primary_color`, inlined into `<style>:root{ ... }`. It goes
    /// in unescaped, so it **must** come from
    /// [`branding::build_brand_css`], which safelists the body.
    pub(crate) tenant_brand_css: Option<String>,
    /// Tables visible in the admin. `None` = every registered model.
    pub(crate) allowed_tables: Option<HashSet<String>>,
    /// Tables whose mutating routes are blocked and whose write
    /// buttons are hidden.
    pub(crate) read_only_tables: HashSet<String>,
    /// When true, **every** visible table is read-only, whatever
    /// `read_only_tables` says. `rustango-tenancy` uses it to gate
    /// non-superuser tenant users without listing every table.
    pub(crate) read_only_all: bool,
    /// Registered bulk action handlers, keyed by table then action
    /// name. `delete_selected` is built in and needs no entry. An
    /// action listed in `admin(actions = "...")` with no handler and
    /// no built-in is rejected at request time.
    pub(crate) actions: AdminActionRegistry,
    /// Permission codenames for the current user. `None` means
    /// superuser: every operation is allowed. `Some(set)` is the
    /// effective set that `is_visible`, `is_read_only`, `can_add`
    /// and `can_delete` check.
    pub(crate) user_perms: Option<HashSet<String>>,
    /// When true, hide registry-scoped models
    /// (`#[rustango(scope = "registry")]`, such as Org and Operator)
    /// from the sidebar and index. A tenant admin runs on the
    /// per-tenant pool, so showing a registry model there can leak
    /// cross-tenant data. `TenantAdminBuilder::build()` sets it.
    /// Single-tenant admins leave it false and see every model.
    pub(crate) tenant_mode: bool,
    /// `Some(operator_id)` when the session is an operator
    /// impersonation. Drives the "you are impersonating" banner and
    /// tags audit entries. `None` for a normal login.
    pub(crate) impersonated_by: Option<i64>,
    /// URL prefix the admin Router is mounted under, passed to every
    /// template as `{{ admin_prefix }}` so links and form actions
    /// resolve. Default `/__admin`; set another with
    /// `Builder::admin_prefix`. An empty string means the admin
    /// router is the root.
    pub(crate) admin_prefix: String,
    /// URL of the self-serve change-password page. When set, the
    /// sidebar links to it. The tenant admin takes it from
    /// `RouteConfig::change_password_url`. Standalone admins with no
    /// auth surface leave it `None`.
    pub(crate) change_password_url: Option<String>,
    /// POST target for the sidebar Logout button. `None` makes the
    /// chrome fall back to `{admin_prefix}/logout`, the bare admin's
    /// own route. The tenant admin sets its
    /// `RouteConfig::logout_url`, which the tenancy layer handles
    /// outside the admin prefix.
    pub(crate) logout_url: Option<String>,
    /// URL suffix for the audit-log view, a sibling of
    /// `admin_prefix`. The feed renders at
    /// `<admin_prefix><audit_url>` and the cleanup form at
    /// `<admin_prefix><audit_url>/cleanup`. Templates read it as
    /// `{{ audit_url }}`. Default `/__audit`; the tenancy admin
    /// takes it from `RouteConfig::audit_url`.
    pub(crate) audit_url: String,
    /// URL prefix for the framework's embedded static assets, the
    /// logo and the favicon. Templates read it as
    /// `{{ static_url }}`, for example
    /// `<link rel="icon" href="{{ static_url }}/icon.png">`. Default
    /// `/__static__`; the tenancy admin takes it from
    /// `RouteConfig::static_url`.
    pub(crate) static_url: String,
    /// Tables whose list view skips `SELECT COUNT(*)` and renders a
    /// "Page N" pager instead of "Page N of M". Needed for tables
    /// with millions of rows, where the count takes seconds even
    /// with indexes. `?count=skip` does the same per request.
    pub(crate) skip_count_tables: HashSet<String>,
    /// Opt-in session auth. When `Some`, the Builder mounts
    /// `/login` and `/logout` and puts every other admin route
    /// behind a valid signed-cookie session. Set it with
    /// [`Builder::with_session_auth`]. `None` means no session auth.
    pub(crate) session_secret: Option<crate::session::SessionSecret>,
    /// When true the admin session cookie carries `Secure`, so it is
    /// sent over HTTPS only. `Builder::new` defaults to `false` for
    /// local plain-HTTP work; `Builder::from_settings` defaults to
    /// `true`. Set it with [`Builder::secure_cookies`].
    pub(crate) secure_cookies: bool,
}

impl Builder {
    pub fn new(pool: impl Into<Pool>) -> Self {
        let pool = pool.into();
        let mut config = Config::default();
        // Defaults for the three URL knobs. Mount the admin
        // elsewhere with `Builder::admin_prefix(...)`; tenancy
        // admins override all three from `RouteConfig`.
        config.admin_prefix = "/__admin".to_owned();
        config.audit_url = "/__audit".to_owned();
        config.static_url = "/__static__".to_owned();
        Self { pool, config }
    }

    /// Build from a parsed [`crate::config::Settings`].
    ///
    /// Order of precedence: the defaults, then the `admin` section,
    /// then the `brand` section, then `routes.admin_url` for the
    /// mount prefix when `admin.url_prefix` is unset. So a deploy can
    /// set `brand.name = "Acme"` once and both the admin and the
    /// operator console use it.
    ///
    /// Builder methods called after this still win. Settings are a
    /// starting point, not a lock.
    #[cfg(feature = "config")]
    pub fn from_settings(pool: impl Into<Pool>, settings: &crate::config::Settings) -> Self {
        let mut builder = Self::new(pool);

        // 1. Admin section: the most specific source.
        let admin = &settings.admin;
        if let Some(t) = admin.title.as_deref() {
            builder = builder.title(t);
        } else if let Some(brand_name) = settings.brand.name.as_deref() {
            // Brand fallback keeps the operator console and the admin
            // chrome in sync without repeating the value.
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

        // 2. URL prefix: `admin.url_prefix` wins, then
        //    `routes.admin_url`, else the `Builder::new` default.
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

        // 3. Visibility and read-only lists.
        if !admin.allowed_tables.is_empty() {
            builder = builder.show_only(admin.allowed_tables.iter().cloned());
        }
        if !admin.read_only_tables.is_empty() {
            builder = builder.read_only(admin.read_only_tables.iter().cloned());
        }

        // 4. Cookie security. Secure by default on the config path;
        //    `security.secure_cookies = false` opts out for local
        //    plain-HTTP work.
        builder = builder.secure_cookies(settings.security.secure_cookies.unwrap_or(true));

        // SSO providers are configured from the admin UI, through the
        // `SsoProvider` model, so there is no settings section here.

        builder
    }

    /// Set whether the admin session cookie carries `Secure`, which
    /// makes it HTTPS-only. `Builder::new` defaults to `false`,
    /// `from_settings` to `true`. Leave it `false` **only** for
    /// local plain-HTTP work: a browser will not send a `Secure`
    /// cookie over HTTP.
    #[must_use]
    pub fn secure_cookies(mut self, secure: bool) -> Self {
        self.config.secure_cookies = secure;
        self
    }

    /// URL prefix the admin Router is mounted under. Templates read
    /// it as `{{ admin_prefix }}`, so links and form actions resolve
    /// under any mount path. Default `/__admin`. Pass an empty
    /// string when the admin is the root router. A trailing slash is
    /// stripped.
    #[must_use]
    pub fn admin_prefix(mut self, prefix: impl Into<String>) -> Self {
        let s: String = prefix.into();
        let trimmed = s.trim_end_matches('/').to_owned();
        self.config.admin_prefix = trimmed;
        self
    }

    /// Opt into signed-cookie session auth. `.build()` then:
    ///
    /// 1. Mounts `/login` (GET and POST) and `/logout` (POST).
    /// 2. Wraps every other admin route in middleware that
    ///    redirects an unauthenticated request to `/login`.
    /// 3. Renders the sidebar "Logout" form.
    ///
    /// Credentials live in the `rustango_admin_users` table
    /// ([`crate::admin::AdminUser`]). Create the table with
    /// [`crate::server::AppBuilder::bootstrap`] or a migration, then
    /// your first operator with
    /// `AdminUser::new_with_password(...).insert(pool)`.
    ///
    /// The signing key is a [`crate::session::SessionSecret`], the
    /// same primitive `tenancy::session` uses, so a host running
    /// both layers can share one key. The cookie names and payloads
    /// differ, so the two layers never decode each other's cookies.
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
        // Point at the standard `/account/password` route so the
        // sidebar's "Change password" link renders. A custom
        // `change_password_url` already set is left alone.
        if self.config.change_password_url.is_none() {
            self.config.change_password_url = Some("/account/password".to_owned());
        }
        self
    }

    // SSO (`admin-sso`) has no builder method. Providers are
    // configured from the admin UI via the `SsoProvider` model; see
    // `crate::admin::sso_provider`. It still needs
    // `Builder::with_session_auth`, because SSO mints the same
    // signed-cookie session, and it links to an existing
    // `AdminUser.email`.

    /// URL suffix the audit-log view is mounted at, a sibling of
    /// `admin_prefix`. A trailing slash is stripped. Default
    /// `/__audit`. Tenant admins set it from
    /// [`crate::tenancy::RouteConfig::audit_url`].
    #[must_use]
    pub fn audit_url(mut self, url: impl Into<String>) -> Self {
        let s: String = url.into();
        let trimmed = s.trim_end_matches('/').to_owned();
        self.config.audit_url = trimmed;
        self
    }

    /// URL prefix for the framework's embedded static assets, the
    /// logo and the favicon. Templates use it to build the favicon
    /// `<link>`. Tenant admins set it from
    /// [`crate::tenancy::RouteConfig::static_url`].
    #[must_use]
    pub fn static_url(mut self, url: impl Into<String>) -> Self {
        let s: String = url.into();
        let trimmed = s.trim_end_matches('/').to_owned();
        self.config.static_url = trimmed;
        self
    }

    /// URL of the self-serve change-password page. When set, the
    /// sidebar renders a "Change password" link to it. Tenant admins
    /// set it from
    /// [`crate::tenancy::RouteConfig::change_password_url`].
    #[must_use]
    pub fn change_password_url(mut self, url: impl Into<String>) -> Self {
        self.config.change_password_url = Some(url.into());
        self
    }

    /// Set the sidebar Logout button's POST target. Omit it and the
    /// chrome uses `{admin_prefix}/logout`. Tenant admins set their
    /// `RouteConfig::logout_url`, so the button hits the
    /// tenancy-layer route instead of a path that does not exist.
    #[must_use]
    pub fn logout_url(mut self, url: impl Into<String>) -> Self {
        self.config.logout_url = Some(url.into());
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

    /// Mark **every** table read-only. List and detail views still
    /// render, but every mutating route returns 403 and the write
    /// buttons are hidden. For callers that gate on a runtime flag,
    /// such as `rustango-tenancy` for non-superuser tenant users,
    /// and do not want to list every table per request.
    pub fn read_only_all(mut self) -> Self {
        self.config.read_only_all = true;
        self
    }

    /// Skip the list view's `SELECT COUNT(*)` for these tables. The
    /// pager renders "Page N" instead of "Page N of M". Needed for
    /// tables with millions of rows, where a filtered `COUNT(*)`
    /// takes seconds.
    ///
    /// Any list URL also accepts `?count=skip` or `?count=0` for the
    /// same effect on one request, with no code change.
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

    /// Mark the current session as an operator impersonation.
    /// `chrome_context` then renders a clear banner and an
    /// "End impersonation" button. `TenantAdminBuilder::build()`
    /// sets it from the validated session cookie.
    #[must_use]
    pub fn impersonated_by(mut self, operator_id: i64) -> Self {
        self.config.impersonated_by = Some(operator_id);
        self
    }

    /// Hide registry-scoped models (`#[rustango(scope =
    /// "registry")]`) from the sidebar and index.
    /// `TenantAdminBuilder::build()` sets it; standalone admins
    /// leave it false. Without it, models like `Org` and `Operator`
    /// show inside a tenant admin, and on a schema-mode tenant
    /// clicking one can **leak cross-tenant data**: the registry's
    /// `public.rustango_orgs` resolves through `search_path`.
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

    /// Per-tenant brand tagline. Like [`Self::brand_name`], it
    /// overrides [`Self::subtitle`] when set.
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

    /// Theme mode: `"light"`, `"dark"` or `"auto"`. Sets the
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
    /// Do **not** call this for a superuser. Leaving it unset means
    /// `None`, which skips every permission check.
    pub fn with_user_perms<I: IntoIterator<Item = String>>(mut self, perms: I) -> Self {
        self.config.user_perms = Some(perms.into_iter().collect());
        self
    }

    /// Register a user-defined bulk action handler.
    ///
    /// `model_table` must match the model's `table = "..."`.
    /// `action_name` **must** also be in that model's
    /// `admin(actions = "...")` allowlist: the attribute is the
    /// allowlist, this is the code that runs.
    ///
    /// The handler gets the pool and the parsed PK list of the
    /// selected rows. Use it for publish, archive, recompute and
    /// anything else that runs over a batch of rows.
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
            // In-admin model reference, like Django's admindocs.
            .route("/__docs", get(super::docs::docs_view))
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

        // Per-model custom views, like Django's
        // `ModelAdmin.get_urls()`. Each inventory entry is checked
        // against the built-in routes, then mounted on the same
        // protected router.
        let protected = mount_custom_views(protected, state.clone());

        // With session auth on, `/login` and `/logout` mount before
        // the auth middleware so they stay reachable. Every other
        // route, including `/account/password`, needs a valid
        // session.
        if let Some(secret) = session_secret {
            use std::sync::Arc as StdArc;
            let gate = super::login_view::SessionGate {
                secret: StdArc::new(secret),
                login_path: if admin_prefix.is_empty() {
                    "/login".to_owned()
                } else {
                    format!("{admin_prefix}/login")
                },
                // The bare admin is superuser-only.
                require_superuser: true,
                // Pool for the per-request password-fingerprint
                // check that revokes sessions after a password
                // change.
                pool: state.pool.clone(),
            };
            // Account routes sit inside the auth middleware, so they
            // need a valid session like the rest of the admin.
            let protected = protected
                .merge(super::login_view::protected_router(state.clone()))
                // **CSRF protection for every admin mutation.**
                // Without it, create, update, delete, bulk actions
                // and audit cleanup all accept a cross-site POST
                // that rides the session cookie, and audit cleanup
                // lets such a request erase its own trace.
                //
                // **Order matters, and it runs in reverse of how it
                // reads.** `route_layer` applies outermost last, so
                // `csrf_context` runs first and mints the token that
                // `chrome_context` reads, then `CsrfLayer`
                // validates, then `require_session` authenticates.
                // The token must exist before any template renders,
                // and validation must happen whether or not the
                // session is valid.
                .route_layer(axum::middleware::from_fn_with_state(
                    gate,
                    super::login_view::require_session,
                ))
                .route_layer(crate::forms::csrf::layer())
                .route_layer(axum::middleware::from_fn(super::csrf_context::csrf_context));
            // Public login and logout routes, plus the per-provider
            // SSO routes. Providers live in the DB (`SsoProvider`),
            // and the routes mount whenever session auth is on,
            // because SSO mints the same signed-cookie session.
            let public = super::login_view::public_router(state.clone());
            #[cfg(feature = "admin-sso")]
            let public = if state.config.session_secret.is_some() {
                public.merge(super::sso::sso_router(state.clone()))
            } else {
                public
            };
            return Router::new().merge(public).merge(protected);
        }

        protected
    }
}

/// Per-request state: the pool plus the resolved `Config`. It is
/// cloned on every request, which is cheap because `Config` is in
/// an `Arc`.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) pool: Pool,
    pub(crate) config: Arc<Config>,
}

impl AppState {
    pub(crate) fn is_visible(&self, table: &str) -> bool {
        // `rustango_admin_users` is the bare admin's credential
        // store, and its table exists only when the host opts into
        // `Builder::with_session_auth`. The derive on `AdminUser`
        // registers it either way, so a tenancy host would see a
        // dead surface in the model index. Hide it instead.
        if table == "rustango_admin_users" && self.config.session_secret.is_none() {
            return false;
        }
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

    /// Scope filter. With `tenant_mode` on, registry-only models
    /// (`#[rustango(scope = "registry")]`, such as `Org` and
    /// `Operator`) are hidden, so **cross-tenant data cannot
    /// surface inside a tenant subdomain**. Standalone admins
    /// return true for every scope.
    pub(crate) fn scope_visible(&self, scope: crate::core::ModelScope) -> bool {
        if !self.config.tenant_mode {
            return true;
        }
        scope == crate::core::ModelScope::Tenant
    }

    /// `true` when the table's edit and update routes are blocked.
    /// Checks the global and per-table read-only flags first, then
    /// `{table}.change` when `user_perms` is set.
    pub(crate) fn is_read_only(&self, table: &str) -> bool {
        if self.config.read_only_all || self.config.read_only_tables.contains(table) {
            return true;
        }
        if let Some(perms) = &self.config.user_perms {
            return !perms.contains(&format!("{table}.change"));
        }
        false
    }

    /// `true` when [`Builder::skip_count_for`] tagged this table,
    /// so the list view skips `SELECT COUNT(*)` and renders a pager
    /// with no total.
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
    /// built-in `delete_selected`, which the caller handles itself,
    /// and for any unregistered name.
    pub(crate) fn action_handler(&self, table: &str, action: &str) -> Option<AdminActionFn> {
        self.config
            .actions
            .get(table)
            .and_then(|m| m.get(action))
            .cloned()
    }
}

/// Mount every registered custom admin view on `router` at
/// `/{table}/{suffix}` with its declared method.
///
/// Skips a view whose suffix collides with a built-in route, and
/// one whose model is not visible to this admin instance.
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
        // A route on a hidden table would be unreachable anyway,
        // and skipping it reads more clearly as "view not loaded".
        if !state.is_visible(view.table) {
            tracing::debug!(
                target: "rustango::admin",
                table = %view.table,
                suffix = %view.suffix,
                "custom admin view registered for a table that is not visible to this admin instance — skipping"
            );
            continue;
        }

        let table = view.table;
        let suffix = view.suffix.trim_start_matches('/');
        let path = format!("/{table}/{suffix}");
        // `view.handler` is a plain fn pointer, so it is Copy.
        let handler = view.handler;
        // Each closure needs its own `Pool`. Clone it here, not
        // inside the closure, so the loop can keep using `state`.
        let pool_for_handler = state.pool.clone();

        let mounted_handler = move |req: Request| {
            let pool = pool_for_handler.clone();
            async move { handler(pool, req).await }
        };

        // Axum has no `MethodFilter` for the QUERY method, so it
        // goes through the `http_query` shim. Everything else maps
        // to a filter.
        let route = if view.method.as_str() == "QUERY" {
            crate::http_query::query(mounted_handler)
        } else {
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
            on(method_filter, mounted_handler)
        };

        router = router.route(&path, route);
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
        // A `PgPool` is awkward to build in a unit test. Connect
        // lazily to a URL that does not exist: none of the methods
        // under test touch the pool.
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
        // A single-tenant project must still see registry-scoped
        // models in its admin.
        let state = state_with(false);
        assert!(state.scope_visible(ModelScope::Tenant));
        assert!(state.scope_visible(ModelScope::Registry));
    }

    #[tokio::test]
    async fn tenant_admin_hides_registry_scoped_models() {
        // A tenant admin must NOT show
        // `#[rustango(scope = "registry")]` models such as Org and
        // Operator. They are not in the tenant pool, and on a
        // schema-mode tenant opening one leaks cross-tenant data
        // through `search_path`.
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

    // The derive always registers `rustango_admin_users`, but its
    // table is created only with `with_session_auth`. A host that
    // never opts in must not see it in the model index.
    #[tokio::test]
    async fn admin_users_hidden_when_session_auth_not_configured() {
        let state = state_with(false);
        // The default Config leaves `session_secret` at None, so
        // the table must be hidden.
        assert!(state.config.session_secret.is_none());
        assert!(!state.is_visible("rustango_admin_users"));
        // Other tables still show.
        assert!(state.is_visible("rustango_users"));
        assert!(state.is_visible("post"));
    }

    #[tokio::test]
    async fn admin_users_visible_when_session_auth_configured() {
        let mut cfg = Config::default();
        cfg.session_secret = Some(crate::session::SessionSecret::from_bytes(vec![0u8; 32]));
        let state = AppState {
            pool: Pool::Postgres(lazy_pg_pool()),
            config: Arc::new(cfg),
        };
        assert!(state.is_visible("rustango_admin_users"));
    }

    // `admin_prefix`: the default is `/__admin`, the setter trims a
    // trailing slash, and an empty string means the admin is the
    // root router.

    #[tokio::test]
    async fn admin_prefix_defaults_to_admin_underscore() {
        let pool = lazy_pg_pool();
        let builder = Builder::new(pool);
        assert_eq!(builder.config.admin_prefix, "/__admin");
    }

    /// `count_skipped_for_table` is true for exactly the tagged
    /// tables. Untagged tables stay on the COUNT path.
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

    /// Repeated `.skip_count_for(...)` calls union the table sets
    /// instead of replacing them, the same as `read_only`.
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

    // Settings should populate every supported knob. Builder calls
    // made after `from_settings` still win.
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
        // With `settings.admin.title` unset, the brand name,
        // tagline and logo_url carry over, so a deploy sets brand
        // once for both the admin and the operator console.
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

    #[tokio::test]
    async fn builder_new_defaults_secure_cookies_false() {
        // The bare path stays plain, often local HTTP, until opted in.
        let b = Builder::new(lazy_pg_pool());
        assert!(!b.config.secure_cookies);
        // Explicit opt-in flips it.
        let b = Builder::new(lazy_pg_pool()).secure_cookies(true);
        assert!(b.config.secure_cookies);
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_defaults_secure_cookies_true() {
        // The config path is secure by default. Only an explicit
        // `security.secure_cookies = false` opts out.
        use crate::config::Settings;
        let settings = Settings::default();
        assert!(settings.security.secure_cookies.is_none());
        let b = Builder::from_settings(lazy_pg_pool(), &settings);
        assert!(b.config.secure_cookies, "secure by default on config path");

        let mut insecure = Settings::default();
        insecure.security.secure_cookies = Some(false);
        let b = Builder::from_settings(lazy_pg_pool(), &insecure);
        assert!(!b.config.secure_cookies, "explicit opt-out honored");
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_admin_url_prefix_wins_over_routes_section() {
        // `admin.url_prefix` is the more specific knob, so it beats
        // `routes.admin_url` when both are set.
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
