//! Admin URL routing: every admin screen's path in one place.
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

/// Path segment for a model's create form. The route below and the
/// post-save redirect in [`super::views`] both read it, so they cannot
/// name different paths (#1635).
pub(crate) const CREATE_SEGMENT: &str = "new";
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
/// The handler takes `&Pool`, not a backend-specific pool, and writes
/// through the ORM. Register it with [`Builder::register_action`]:
///
/// ```no_run
/// use rustango::admin::{AdminActionFuture, Builder};
/// use rustango::core::{Op, SqlValue};
/// use rustango::sql::{Pool, UpdaterPool as _};
/// use rustango::Model;
///
/// #[derive(Model)]
/// #[rustango(table = "post")]
/// pub struct Post {
///     #[rustango(primary_key)]
///     pub id: i64,
///     pub published_at: Option<chrono::DateTime<chrono::Utc>>,
/// }
///
/// fn publish<'a>(pool: &'a Pool, pks: &'a [SqlValue]) -> AdminActionFuture<'a> {
///     Box::pin(async move {
///         Post::objects()
///             .filter_op("id", Op::In, SqlValue::List(pks.to_vec()))
///             .update()
///             .set("published_at", chrono::Utc::now())
///             .execute_pool(pool)
///             .await?;
///         Ok(())
///     })
/// }
///
/// fn admin(pool: Pool) -> Builder {
///     Builder::new(pool).register_action("post", "publish", publish)
/// }
/// # fn main() {}
/// ```
pub type AdminActionFn = Arc<
    dyn for<'a> Fn(&'a crate::sql::Pool, &'a [SqlValue]) -> AdminActionFuture<'a> + Send + Sync,
>;

/// Bulk action handler that writes in the admin's transaction, so the
/// action's audit rows commit or roll back with it. See
/// [`Builder::register_tx_action`].
pub type AdminTxActionFn = Arc<
    dyn for<'a> Fn(&'a mut crate::sql::PoolTx<'static>, &'a [SqlValue]) -> AdminActionFuture<'a>
        + Send
        + Sync,
>;

/// How a custom action writes: on its own connection, or in the admin's tx.
#[derive(Clone)]
pub(crate) enum ActionHandler {
    Pool(AdminActionFn),
    Tx(AdminTxActionFn),
}

/// The permission a custom bulk action is checked against (#1818).
///
/// Any custom action also needs the table writable, so `Delete` needs
/// `{table}.change` too. It audits as an update and sends edit signals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionPerm {
    /// An edit: `{table}.change` and the `change` object hook.
    #[default]
    Change,
    /// A delete: `{table}.delete` and the `delete` object hook.
    Delete,
}

/// A registered custom action: its handler and the permission it needs.
#[derive(Clone)]
pub(crate) struct RegisteredAction {
    pub(crate) handler: ActionHandler,
    pub(crate) perm: ActionPerm,
}

/// Action registry: table name, then action name, then action. The
/// action name **must** also be in the model's
/// `admin(actions = "...")` allowlist. That attribute is the
/// allowlist; this registry only holds the callables.
pub(crate) type AdminActionRegistry =
    HashMap<&'static str, HashMap<&'static str, RegisteredAction>>;

/// Set once this process builds an admin without
/// [`Builder::with_session_auth`]; read by `check --deploy` (#1627).
static UNGATED_ADMIN_BUILT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `true` when this process built an admin anyone can read and write.
pub(crate) fn ungated_admin_built() -> bool {
    UNGATED_ADMIN_BUILT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Clear the flag. Callers hold [`ungated_flag_lock`].
#[cfg(all(test, feature = "sqlite"))]
pub(crate) fn reset_ungated_admin_built() {
    UNGATED_ADMIN_BUILT.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Serialises the tests that read or write the process-wide flag.
#[cfg(all(test, feature = "sqlite"))]
pub(crate) fn ungated_flag_lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

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

/// The database an admin serves. Each holds a different set of tables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AdminDatabase {
    /// A single-database app: `migrate` runs only the tenant-scope system
    /// chain, so registry-only tables such as `rustango_orgs` are absent (#2365).
    #[default]
    Single,
    /// A tenancy registry, set by [`Builder::registry_mode`].
    Registry,
    /// One tenant's storage, set by [`Builder::tenant_mode`]. Registry
    /// models stay out: on a schema-mode tenant `search_path` would reach
    /// the registry's copy and leak cross-tenant data.
    Tenant,
}

impl AdminDatabase {
    /// Whether this database holds `schema`'s table.
    fn holds(self, schema: &crate::core::ModelSchema) -> bool {
        use crate::core::ModelScope;
        use crate::migrate::snapshot::in_system_chain;
        match self {
            Self::Single => {
                !in_system_chain(schema, ModelScope::Registry)
                    || in_system_chain(schema, ModelScope::Tenant)
            }
            Self::Registry => {
                schema.scope == ModelScope::Registry
                    || in_system_chain(schema, ModelScope::Registry)
            }
            Self::Tenant => schema.scope == ModelScope::Tenant,
        }
    }
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
    /// Public URL of the tenant logo, e.g. `/_brand/{slug}/logo.png`.
    pub(crate) brand_logo_url: Option<String>,
    /// Public URL of the tenant favicon. `None` keeps the bundled icon.
    pub(crate) brand_favicon_url: Option<String>,
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
    /// The database this admin serves, which decides the models it lists.
    pub(crate) database: AdminDatabase,
    /// The tenancy layer gates every route around this admin, so
    /// `check --deploy` must not call it ungated (#1627).
    pub(crate) gated_upstream: bool,
    /// `Some(operator_id)` when the session is an operator
    /// impersonation. Drives the "you are impersonating" banner and
    /// tags audit entries. `None` for a normal login.
    pub(crate) impersonated_by: Option<i64>,
    /// The tenant's slug, for the admin signals; `None` outside a tenant.
    pub(crate) tenant: Option<String>,
    /// URL prefix the admin Router is mounted under, passed to every
    /// template as `{{ admin_prefix }}` so links and form actions
    /// resolve. Default `/__admin`; set another with
    /// `Builder::admin_prefix`. An empty string means the admin
    /// router is the root.
    pub(crate) admin_prefix: String,
    /// Full URL of the self-serve change-password page; the sidebar links
    /// to it as is. `None` with session auth means `{admin_prefix}/account/password`.
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
    /// Most rows a list scans to run a `"view"` hook. `None`: 10 000.
    pub(crate) view_hook_scan_cap: Option<usize>,
    /// Opt-in session auth. When `Some`, the Builder mounts
    /// `/login` and `/logout` and puts every other admin route
    /// behind a valid signed-cookie session. Set it with
    /// [`Builder::with_session_auth`]. `None` means no session auth.
    pub(crate) session_secret: Option<crate::session::SessionSecret>,
    /// When true the admin session cookie carries `Secure`, so it is
    /// sent over HTTPS only. `Builder::new` follows
    /// [`crate::session::secure_cookies`]; `Builder::from_settings`
    /// defaults to `true`. Set it with [`Builder::secure_cookies`].
    pub(crate) secure_cookies: bool,
    /// Hosts the admin builds absolute URLs for (the SSO `redirect_uri`).
    /// `None`: the request's Host. Set it with [`Builder::allowed_hosts`].
    #[cfg_attr(not(feature = "admin-sso"), allow(dead_code))]
    pub(crate) allowed_hosts: Option<crate::host_validation::AllowedHostsLayer>,
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
        // The process policy: `[security].secure_cookies`, else the prod tier.
        config.secure_cookies = crate::session::secure_cookies();
        let b = Self { pool, config };
        #[cfg(feature = "sso")]
        let b = crate::sso::policy::admin::register_link_actions(b);
        b
    }

    /// The mount prefix `settings` asks for, without a trailing slash:
    /// `admin.url_prefix`, else `routes.admin_url`. Nest the admin router here.
    /// `"/"` gives `""`, the root, which `Cli::nest_with` refuses at boot.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn settings_prefix(settings: &crate::config::Settings) -> Option<&str> {
        settings
            .admin
            .url_prefix
            .as_deref()
            .or(settings.routes.admin_url.as_deref())
            .map(|p| p.trim_end_matches('/'))
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

        // 2. URL prefix, else the `Builder::new` default.
        if let Some(prefix) = Self::settings_prefix(settings) {
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
        builder = builder.allowed_hosts(&settings.security.allowed_hosts);

        // SSO providers are configured from the admin UI, through the
        // `SsoProvider` model, so there is no settings section here.

        builder
    }

    /// Set whether the admin session cookie carries `Secure`, which
    /// makes it HTTPS-only. `Builder::new` follows
    /// [`crate::session::secure_cookies`], `from_settings` defaults to
    /// `true`. Leave it `false` **only** for
    /// local plain-HTTP work: a browser will not send a `Secure`
    /// cookie over HTTP.
    #[must_use]
    pub fn secure_cookies(mut self, secure: bool) -> Self {
        self.config.secure_cookies = secure;
        self
    }

    /// Hosts (`Settings.security.allowed_hosts` patterns) the admin builds
    /// absolute URLs for, such as the SSO `redirect_uri`; another Host is
    /// refused there. An empty list keeps the request's Host.
    /// `from_settings` takes `security.allowed_hosts`.
    #[must_use]
    pub fn allowed_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let hosts: Vec<String> = hosts
            .into_iter()
            .map(|h| h.as_ref().trim().to_owned())
            .filter(|h| !h.is_empty())
            .collect();
        self.config.allowed_hosts =
            (!hosts.is_empty()).then(|| crate::host_validation::AllowedHostsLayer::new(hosts));
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

    /// Full URL of the self-serve change-password page, linked as given
    /// (not under the admin prefix) from the sidebar. Tenant admins
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

    /// Mark these tables read-only. List/detail still render; the edit
    /// form redirects to the detail page; create, update and delete
    /// return 403, and the corresponding buttons are hidden in the HTML.
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

    /// Most rows a list view reads to run a `"view"` object-permission
    /// hook (default 10 000). Past it the total, facets and date strip are
    /// withheld rather than count rows the hook denies.
    #[must_use]
    pub fn view_hook_scan_cap(mut self, rows: usize) -> Self {
        self.config.view_hook_scan_cap = Some(rows);
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

    /// The tenant this admin serves, named in its signals.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub(crate) fn tenant(mut self, slug: &str) -> Self {
        self.config.tenant = Some(slug.to_owned());
        self
    }

    /// Serve one tenant: list only tenant-scope models.
    /// `TenantAdminBuilder::build()` sets it. On a schema-mode tenant a
    /// registry model would **leak cross-tenant data** through `search_path`.
    /// The last of `tenant_mode` / `registry_mode` called wins.
    #[must_use]
    pub fn tenant_mode(mut self) -> Self {
        self.config.database = AdminDatabase::Tenant;
        self
    }

    /// Serve a tenancy registry: list registry models such as `Org` and
    /// `Operator`. A plain admin is a single-database one and hides them,
    /// because single-database `migrate` never creates their tables.
    /// The last of `tenant_mode` / `registry_mode` called wins.
    #[must_use]
    pub fn registry_mode(mut self) -> Self {
        self.config.database = AdminDatabase::Registry;
        self
    }

    /// Mark this admin as gated by the tenancy layer around it.
    #[cfg(feature = "tenancy")]
    pub(crate) fn gated_upstream(mut self) -> Self {
        self.config.gated_upstream = true;
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

    /// Public URL of the favicon. Unset keeps the bundled icon.
    #[must_use]
    pub fn brand_favicon_url(mut self, url: impl Into<String>) -> Self {
        self.config.brand_favicon_url = Some(url.into());
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
    ///   read-only (the edit link opens the detail page; save returns 403).
    /// * `{table}.add` gates the create form and create submit.
    /// * `{table}.delete` gates delete submit and `delete_selected`.
    ///
    /// Do **not** call this for a superuser. Leaving it unset means
    /// `None`, which skips every permission check. Set, the user is not a
    /// superuser even with a superuser session: superuser-only fields lock.
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
    /// It is gated like an edit: `{table}.change` plus the object hook
    /// named after the action. An action that deletes should use
    /// [`Self::register_action_with_perm`] with [`ActionPerm::Delete`].
    ///
    /// Its audit rows are written after it, best effort. Use
    /// [`Self::register_tx_action`] to audit in the action's transaction.
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
        self,
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
        self.register_action_with_perm(model_table, action_name, ActionPerm::Change, handler)
    }

    /// [`Self::register_action`] checked against `perm` instead of
    /// `change`, plus the object hook named after the action (#1818).
    /// A `Delete` action still needs `{table}.change`, and it audits and
    /// signals as an edit; the handler does its own deleting.
    pub fn register_action_with_perm<F>(
        self,
        model_table: &'static str,
        action_name: &'static str,
        perm: ActionPerm,
        handler: F,
    ) -> Self
    where
        F: for<'a> Fn(&'a crate::sql::Pool, &'a [SqlValue]) -> AdminActionFuture<'a>
            + Send
            + Sync
            + 'static,
    {
        self.insert_action(
            model_table,
            action_name,
            perm,
            ActionHandler::Pool(Arc::new(handler)),
        )
    }

    /// [`Self::register_action`] for a handler that writes in the admin's
    /// transaction: the action's audit rows commit with its writes, and an
    /// error from either rolls both back. Prefer it for audited models.
    ///
    /// ```ignore
    /// admin::Builder::new(pool)
    ///     .register_tx_action("post", "publish", |tx, pks| {
    ///         Box::pin(async move {
    ///             rustango::sql::update_tx(tx, &publish_query(pks)).await
    ///                 .map_err(|e| AdminError::Internal(e.to_string()))?;
    ///             Ok(())
    ///         })
    ///     })
    ///     .build();
    /// ```
    pub fn register_tx_action<F>(
        self,
        model_table: &'static str,
        action_name: &'static str,
        handler: F,
    ) -> Self
    where
        F: for<'a> Fn(&'a mut crate::sql::PoolTx<'static>, &'a [SqlValue]) -> AdminActionFuture<'a>
            + Send
            + Sync
            + 'static,
    {
        self.register_tx_action_with_perm(model_table, action_name, ActionPerm::Change, handler)
    }

    /// [`Self::register_tx_action`] checked against `perm`, as
    /// [`Self::register_action_with_perm`].
    pub fn register_tx_action_with_perm<F>(
        self,
        model_table: &'static str,
        action_name: &'static str,
        perm: ActionPerm,
        handler: F,
    ) -> Self
    where
        F: for<'a> Fn(&'a mut crate::sql::PoolTx<'static>, &'a [SqlValue]) -> AdminActionFuture<'a>
            + Send
            + Sync
            + 'static,
    {
        self.insert_action(
            model_table,
            action_name,
            perm,
            ActionHandler::Tx(Arc::new(handler)),
        )
    }

    fn insert_action(
        mut self,
        model_table: &'static str,
        action_name: &'static str,
        perm: ActionPerm,
        handler: ActionHandler,
    ) -> Self {
        self.config
            .actions
            .entry(model_table)
            .or_default()
            .insert(action_name, RegisteredAction { handler, perm });
        self
    }

    pub fn build(mut self) -> Router {
        if self.config.session_secret.is_none() && !self.config.gated_upstream {
            UNGATED_ADMIN_BUILT.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // A path `static_url` is served by this router, so its links carry the mount.
        let mut assets = Router::new();
        if self.config.static_url.starts_with('/') {
            let icon = format!("{}/icon.png", self.config.static_url);
            assets = assets.route(&icon, get(|| async { crate::welcome::icon_png_response() }));
            self.config.static_url =
                format!("{}{}", self.config.admin_prefix, self.config.static_url);
        }
        let audit_path = self.config.audit_url.clone();
        let session_secret = self.config.session_secret.clone();
        let admin_prefix = self.config.admin_prefix.clone();
        let state = AppState {
            pool: self.pool,
            config: Arc::new(self.config),
        };
        let protected = protected_routes(&audit_path, StateSource::Fixed(state.clone()), &|t| {
            state.is_visible(t)
        });

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
                logout_path: format!("{admin_prefix}/logout"),
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
                // `CsrfLayer` validates first, then `csrf_context`
                // mints the token that `chrome_context` reads, then
                // `require_session` authenticates. The layer sits
                // outside so it sees `csrf_context`'s cookie and adds
                // no second one (#1711).
                .route_layer(axum::middleware::from_fn_with_state(
                    gate,
                    super::login_view::require_session,
                ))
                .route_layer(axum::middleware::from_fn(super::csrf_context::csrf_context))
                .route_layer(crate::forms::csrf::layer());
            // Public login and logout routes, plus the per-provider
            // SSO routes. Providers live in the DB (`SsoProvider`),
            // and the routes mount whenever session auth is on,
            // because SSO mints the same signed-cookie session.
            let public = super::login_view::public_router(state.clone());
            #[cfg(feature = "admin-sso")]
            let public = if state.config.session_secret.is_some() {
                if state.config.allowed_hosts.is_none() {
                    tracing::warn!(
                        target: "rustango::admin::sso",
                        "admin SSO builds its redirect URI from any Host: set security.allowed_hosts"
                    );
                }
                public.merge(super::sso::sso_router(state.clone()))
            } else {
                public
            };
            return crate::errors::own_pages(assets.merge(public).merge(protected));
        }

        crate::errors::own_pages(assets.merge(protected))
    }
}

#[cfg(feature = "tenancy")]
impl Builder {
    /// This builder's settings as one request's state, for a
    /// [`per_request_router`].
    pub(crate) fn into_request_state(self) -> RequestState {
        RequestState(AppState {
            pool: self.pool,
            config: Arc::new(self.config),
        })
    }
}

/// Where the admin's handlers get their [`AppState`]. Handlers extract
/// `AppState` itself, so none can read a state the request did not pick.
#[derive(Clone)]
pub(crate) enum StateSource {
    /// One state for every request: [`Builder::build`].
    Fixed(AppState),
    /// Each request brings its own [`RequestState`]; one without is a 500.
    #[cfg(feature = "tenancy")]
    PerRequest,
}

/// One request's admin state: its pool, user perms and branding.
#[cfg(feature = "tenancy")]
#[derive(Clone)]
pub(crate) struct RequestState(AppState);

impl axum::extract::FromRequestParts<StateSource> for AppState {
    type Rejection = AdminError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        source: &StateSource,
    ) -> Result<Self, AdminError> {
        #[cfg(not(feature = "tenancy"))]
        let _ = parts;
        match source {
            StateSource::Fixed(state) => Ok(state.clone()),
            #[cfg(feature = "tenancy")]
            StateSource::PerRequest => parts
                .extensions
                .get::<RequestState>()
                .map(|s| s.0.clone())
                .ok_or_else(|| AdminError::Internal("admin request carries no state".into())),
        }
    }
}

/// Route tables built so far, for the build-once guard.
static ROUTER_BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[doc(hidden)]
#[must_use]
pub fn __router_builds() -> usize {
    ROUTER_BUILDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The admin routes for a caller that builds them once and puts a
/// [`RequestState`] in each request.
#[cfg(feature = "tenancy")]
pub(crate) fn per_request_router(audit_url: &str) -> Router {
    // Visibility depends on the request's perms, so every view mounts and
    // checks it per request.
    protected_routes(audit_url, StateSource::PerRequest, &|_| true)
}

/// Every admin route behind the session gate. `mount_view` picks the
/// custom views worth mounting; each still checks access per request.
fn protected_routes(
    audit_path: &str,
    source: StateSource,
    mount_view: &dyn Fn(&str) -> bool,
) -> Router {
    ROUTER_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let audit_cleanup_path = format!("{audit_path}/cleanup");
    let routes = Router::new()
        .route("/", get(views::index))
        .route(audit_path, get(super::audit::audit_log_view))
        .route(
            &audit_cleanup_path,
            post(super::audit::audit_cleanup_submit),
        )
        // In-admin model reference: every registered model.
        .route("/__docs", get(super::docs::docs_view))
        .route(
            "/{table}",
            get(views::table_view).post(views::create_submit),
        )
        .route(
            &format!("/{{table}}/{CREATE_SEGMENT}"),
            get(views::create_form),
        )
        .route("/{table}/__action", post(views::action_submit))
        .route("/{table}/__autocomplete", get(views::autocomplete_view))
        .route(
            "/{table}/{pk}",
            get(views::detail_view).post(views::update_submit),
        )
        .route("/{table}/{pk}/edit", get(views::edit_form))
        .route("/{table}/{pk}/delete", post(views::delete_submit));
    // Extra routes a model registers for itself. Each inventory
    // entry is checked against the built-in routes, then mounted
    // on the same protected router.
    mount_custom_views(routes, mount_view)
        .layer(axum::middleware::from_fn(
            super::session::scope_request_session,
        ))
        .layer(axum::middleware::from_fn_with_state(
            source.clone(),
            mark_perms_scoped,
        ))
        .with_state(source)
}

/// Tag the request with whether its state carries per-user perms.
async fn mark_perms_scoped(
    state: AppState,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(super::session::PermsScoped(
        state.config.user_perms.is_some(),
    ));
    next.run(req).await
}

/// Tables the generic admin never serves: the TOTP store holds raw
/// secrets, and a passkey row signs in as its `user_id` (#2364).
/// Enrollment has its own pages.
fn is_never_served(table: &str) -> bool {
    #[cfg(not(any(feature = "totp", feature = "passkey", feature = "tenancy")))]
    let _ = table;
    // Written only through `db_server`, which seals the admin password.
    #[cfg(feature = "tenancy")]
    if table == <crate::tenancy::DbServer as crate::core::Model>::SCHEMA.table {
        return true;
    }
    // A purge drops what these rows name, so only provisioning writes them.
    #[cfg(feature = "tenancy")]
    if table == <crate::tenancy::TenantDatabase as crate::core::Model>::SCHEMA.table {
        return true;
    }
    #[cfg(feature = "totp")]
    if table == <super::totp_store::AdminTotp as crate::core::Model>::SCHEMA.table {
        return true;
    }
    #[cfg(all(feature = "tenancy", feature = "totp"))]
    if table == <crate::tenancy::OperatorTotp as crate::core::Model>::SCHEMA.table {
        return true;
    }
    #[cfg(feature = "passkey")]
    if table == <crate::passkey::WebauthnCredential as crate::core::Model>::SCHEMA.table {
        return true;
    }
    false
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
    /// Fields this user may not write on `action`.
    pub(crate) fn locked_fields(
        &self,
        model: &'static crate::core::ModelSchema,
        action: super::object_permissions::AdminWrite,
    ) -> super::object_permissions::LockedFields {
        let superuser_only = if super::session::acting_superuser(self.config.user_perms.is_some()) {
            Vec::new()
        } else {
            super::object_permissions::superuser_fields(model.table, action)
        };
        super::object_permissions::LockedFields {
            readonly: super::helpers::admin_config_or_default(model).readonly_fields,
            superuser_only,
        }
    }

    /// Whether this admin serves `table` at all. Index, sidebar, routes,
    /// custom views and docs all ask here, so they cannot disagree.
    pub(crate) fn is_visible(&self, table: &str) -> bool {
        use crate::core::Model as _;
        // Only tables this database holds (#2360, #2365).
        if super::helpers::served_entry(table)
            .is_some_and(|e| !self.config.database.holds(e.schema))
        {
            return false;
        }
        if is_never_served(table) {
            return false;
        }
        // `rustango_admin_users` is the bare admin's credential
        // store, and its table exists only when the host opts into
        // `Builder::with_session_auth`. The derive on `AdminUser`
        // registers it either way, so a tenancy host would see a
        // dead surface in the model index. Hide it instead.
        if self.config.session_secret.is_none() && table == super::user::AdminUser::SCHEMA.table {
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

    /// See [`Builder::view_hook_scan_cap`].
    pub(crate) fn view_hook_scan_cap(&self) -> usize {
        self.config.view_hook_scan_cap.unwrap_or(10_000)
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

    /// `true` when the user holds `{table}.<perm>`: `view`, `change`,
    /// `add` and `delete` follow the flags above; any other codename
    /// needs the perm itself. No perm set is a superuser, who holds all.
    pub(crate) fn allows(&self, table: &str, perm: &str) -> bool {
        match perm {
            "view" => self.is_visible(table),
            "change" => !self.is_read_only(table),
            "add" => self.can_add(table),
            "delete" => self.can_delete(table),
            other => self
                .config
                .user_perms
                .as_ref()
                .is_none_or(|perms| perms.contains(&format!("{table}.{other}"))),
        }
    }

    /// Look up a registered action handler. Returns `None` for the
    /// built-in `delete_selected`, which the caller handles itself,
    /// and for any unregistered name.
    pub(crate) fn action_handler(&self, table: &str, action: &str) -> Option<RegisteredAction> {
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
fn mount_custom_views(
    mut router: Router<StateSource>,
    mount_view: &dyn Fn(&str) -> bool,
) -> Router<StateSource> {
    use axum::extract::Request;
    use axum::http::Method;
    use axum::response::IntoResponse as _;
    use axum::routing::on;
    use axum::routing::MethodFilter;

    for view in inventory::iter::<super::custom_views::AdminCustomView> {
        match super::custom_views::clash(view.suffix) {
            Some(super::custom_views::Clash::Reserved) => {
                tracing::warn!(
                    target: "rustango::admin",
                    table = %view.table,
                    suffix = %view.suffix,
                    "custom admin view suffix collides with a built-in admin route — skipping"
                );
                continue;
            }
            Some(super::custom_views::Clash::Shadows(built_in)) => {
                tracing::warn!(
                    target: "rustango::admin",
                    table = %view.table,
                    suffix = %view.suffix,
                    built_in = %built_in,
                    "custom admin view shadows a built-in admin route — skipping; rename its suffix"
                );
                continue;
            }
            None => {}
        }
        // A hidden table, registry ones on a tenant admin included,
        // gets no custom routes either.
        if !mount_view(view.table) {
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
        // A user without the view's perm gets 403 rather than the handler.
        // The method default applies only under `with_user_perms`.
        let perm = view.required_perm();
        let view_perm = view.perm;
        let mounted_handler = move |state: AppState, req: Request| async move {
            if !state.is_visible(table) {
                return AdminError::TableNotFound {
                    table: table.to_owned(),
                }
                .into_response();
            }
            let allowed = (view_perm.is_none() && state.config.user_perms.is_none())
                || state.allows(table, perm);
            if !allowed {
                return AdminError::Forbidden {
                    table: table.to_owned(),
                    action: perm,
                }
                .into_response();
            }
            handler(state.pool, req).await
        };

        // Axum has no `MethodFilter` for the QUERY method, so it
        // goes through the `http_query` shim. Everything else maps
        // to a filter.
        let method = view.mount_method();
        if method != view.method {
            tracing::warn!(
                target: "rustango::admin",
                table = %view.table,
                suffix = %view.suffix,
                method = ?view.method,
                "custom admin view declared with an unsupported HTTP method — defaulting to GET"
            );
        }
        let route = if method.as_str() == "QUERY" {
            crate::http_query::query(mounted_handler)
        } else {
            let method_filter = match method {
                Method::POST => MethodFilter::POST,
                Method::PUT => MethodFilter::PUT,
                Method::DELETE => MethodFilter::DELETE,
                Method::PATCH => MethodFilter::PATCH,
                _ => MethodFilter::GET,
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
    use sqlx::PgPool;
    use std::sync::Arc;

    fn lazy_pg_pool() -> sqlx::PgPool {
        // A `PgPool` is awkward to build in a unit test. Connect
        // lazily to a URL that does not exist: none of the methods
        // under test touch the pool.
        PgPool::connect_lazy("postgres://_:_@127.0.0.1:1/_unused")
            .expect("connect_lazy never fails")
    }

    fn state_with(database: AdminDatabase) -> AppState {
        let mut cfg = Config::default();
        cfg.database = database;
        AppState {
            pool: Pool::Postgres(lazy_pg_pool()),
            config: Arc::new(cfg),
        }
    }

    #[tokio::test]
    async fn single_database_admin_hides_registry_only_tables() {
        // Single-database `migrate` never creates them (#2365).
        let state = state_with(AdminDatabase::Single);
        #[cfg(feature = "tenancy")]
        assert!(!state.is_visible("rustango_orgs"));
        // Unmanaged and tenant-scope framework tables are there.
        assert!(state.is_visible("rustango_translations"));
        assert!(state.is_visible("rustango_users"));
        // A registry holds its own tables and no tenant ones.
        #[cfg(feature = "tenancy")]
        {
            let registry = state_with(AdminDatabase::Registry);
            assert!(registry.is_visible("rustango_orgs"));
            assert!(!registry.is_visible("rustango_users"));
            // Its password column must be written only sealed.
            assert!(!registry.is_visible("rustango_db_servers"));
            // A purge drops what its rows name.
            assert!(!registry.is_visible("rustango_tenant_databases"));
            assert!(registry.is_visible("rustango_audit_log"));
        }
    }

    #[tokio::test]
    async fn tenant_admin_hides_registry_scoped_models() {
        // A tenant admin must NOT show
        // `#[rustango(scope = "registry")]` models such as Org and
        // Operator. They are not in the tenant pool, and on a
        // schema-mode tenant opening one leaks cross-tenant data
        // through `search_path`.
        let state = state_with(AdminDatabase::Tenant);
        assert!(state.is_visible("rustango_users"));
        #[cfg(feature = "tenancy")]
        assert!(!state.is_visible("rustango_orgs"));
        // `is_visible` is the gate custom views and docs use too (#2360).
        assert!(!state.is_visible("rustango_translations"));
    }

    #[tokio::test]
    async fn mode_setters_pick_the_database() {
        let pool = PgPool::connect_lazy("postgres://_:_@127.0.0.1:1/_unused")
            .expect("connect_lazy never fails");
        let builder = || Builder::new(pool.clone());
        assert_eq!(builder().config.database, AdminDatabase::Single);
        assert_eq!(
            builder().tenant_mode().config.database,
            AdminDatabase::Tenant
        );
        assert_eq!(
            builder().registry_mode().config.database,
            AdminDatabase::Registry
        );
    }

    // The derive always registers `rustango_admin_users`, but its
    // table is created only with `with_session_auth`. A host that
    // never opts in must not see it in the model index.
    #[tokio::test]
    async fn admin_users_hidden_when_session_auth_not_configured() {
        let state = state_with(AdminDatabase::Single);
        // The default Config leaves `session_secret` at None, so
        // the table must be hidden.
        assert!(state.config.session_secret.is_none());
        assert!(!state.is_visible("rustango_admin_users"));
        #[cfg(feature = "totp")]
        assert!(!state.is_visible("rustango_admin_totp"));
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
        // Raw TOTP secrets never show in the generic admin.
        #[cfg(feature = "totp")]
        assert!(!state.is_visible("rustango_admin_totp"));
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
    async fn builder_new_follows_the_secure_cookie_policy() {
        // The override is a `OnceLock` and nextest gives each test its own
        // process; if something set it first, only assert on a secure policy.
        let ours = crate::session::set_secure_cookies(true);
        if ours || crate::session::secure_cookies() {
            assert!(Builder::new(lazy_pg_pool()).config.secure_cookies);
        }
        // An explicit call still wins.
        let b = Builder::new(lazy_pg_pool()).secure_cookies(false);
        assert!(!b.config.secure_cookies);
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
