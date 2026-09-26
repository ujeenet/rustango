//! Tenant-aware admin: wraps `rustango-admin` with per-request
//! resolver dispatch.
//!
//! ```ignore
//! let app = Router::new()
//!     .nest("/operator", rustango::admin::router(pools.registry().clone()))
//!     .merge(crate::tenancy::admin::TenantAdminBuilder::new(
//!         pools.clone(),
//!         registry_url,
//!         ChainResolver::standard("app.example.com"),
//!     ).read_only(["audit_log"]).build());
//! ```
//!
//! Per request:
//!
//! 1. The resolver runs against the request parts and the registry.
//! 2. No tenant means 404.
//! 3. Otherwise `TenantPools::scoped_pool_dyn` gives a pool for that
//!    tenant. Database-mode tenants reuse a cached pool. Schema-mode
//!    Postgres tenants get a short-lived pool with `search_path`
//!    already set, dropped when the request ends.
//! 4. A one-shot `rustango-admin` router runs on that pool and its
//!    response is returned as-is.
//!
//! Resolution is cached (see [`super::resolver_cache`]), so the
//! registry lookup is not per request.
//!
//! ## Per-tenant auth
//!
//! Opt in with `TenantAdminBuilder::with_session(SessionSecret)`:
//!
//! * Anonymous traffic is redirected to `/__login`.
//! * `POST /__login` calls `auth::authenticate_user` on the resolved
//!   tenant's pool and issues a signed cookie.
//! * Superusers get the full read/write admin.
//! * Everyone else gets a read-only admin: lists and detail pages
//!   render, mutating routes 403, write buttons are hidden.
//!
//! Keeping the operator UI off the apex is still the caller's job;
//! dispatch on host, as `multitenant_demo` does.

use std::marker::PhantomData;
use std::sync::Arc;

use crate::sql::sqlx::Database;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Router;
use cookie::time::Duration as CookieDuration;
use cookie::{Cookie, SameSite};
use tera::{Context, Tera};
use tower::ServiceExt;
use tracing::warn;

use super::branding;
use super::org::Org;
use super::pools::{DefaultTenantDb, TenantPools};
use super::resolver::OrgResolver;
use super::tenant_console::{self, TenantSessionPayload};
use crate::storage::BoxedStorage;

/// Builder for the tenant-aware admin router.
///
/// Generic over the backend, defaulting to Postgres so existing call
/// sites need no turbofish. `TenantAdminBuilder<sqlx::Sqlite>` and
/// `TenantAdminBuilder<sqlx::MySql>` work too.
///
/// Schema mode is Postgres-only: on another backend
/// `TenantPools::scoped_pool_dyn` rejects a schema-mode tenant, so
/// that arm is never reached.
pub struct TenantAdminBuilder<DB: Database = DefaultTenantDb> {
    pools: Arc<TenantPools<DB>>,
    registry_url: String,
    resolver: Arc<dyn OrgResolver>,
    show_only: Option<Vec<String>>,
    read_only: Vec<String>,
    session: Option<Arc<TenantSessionConfig>>,
    actions: Vec<RegisteredAction>,
    title: Option<String>,
    subtitle: Option<String>,
    brand_storage: Option<BoxedStorage>,
    /// URL prefixes. Defaults to `RouteConfig::default()`.
    routes: Arc<super::routes::RouteConfig>,
    _phantom: PhantomData<DB>,
}

/// One registered admin action. Re-applied on every request, when the
/// inner admin router is built for the resolved tenant.
#[derive(Clone)]
struct RegisteredAction {
    table: &'static str,
    name: &'static str,
    handler: crate::admin::AdminActionFn,
}

struct TenantSessionConfig {
    secret: tenant_console::SessionSecret,
    tera: Tera,
}

impl<DB: Database> TenantAdminBuilder<DB> {
    /// Build a tenant-aware admin handler.
    ///
    /// `registry_url` is used to open short-lived schema-mode admin
    /// pools. Database-mode tenants get their pool from `TenantPools`
    /// instead, so any valid URL will do if you have only those.
    #[must_use]
    pub fn new(
        pools: Arc<TenantPools<DB>>,
        registry_url: impl Into<String>,
        resolver: impl OrgResolver,
    ) -> Self {
        Self {
            pools,
            registry_url: registry_url.into(),
            resolver: Arc::new(resolver),
            show_only: None,
            read_only: Vec::new(),
            session: None,
            actions: Vec::new(),
            title: None,
            subtitle: None,
            brand_storage: None,
            routes: Arc::new(super::routes::RouteConfig::default()),
            _phantom: PhantomData,
        }
    }

    /// Override the default URL prefixes. If your app also mounts the
    /// operator console, give it the same `RouteConfig` so both sides
    /// agree on `/login`, `/admin` and the rest.
    #[must_use]
    pub fn routes(mut self, routes: super::routes::RouteConfig) -> Self {
        self.routes = Arc::new(routes);
        self
    }

    /// Set the display name shown in the admin sidebar header.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set an optional subtitle shown below the title.
    #[must_use]
    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Choose where per-tenant brand assets are stored: any
    /// [`BoxedStorage`], such as `LocalStorage`, `S3Storage` or
    /// `InMemoryStorage` for tests.
    ///
    /// If the backend returns a URL from `Storage::url`, `<img src>`
    /// points straight at it and nothing proxies through this process.
    /// The default is [`super::branding::default_brand_storage`].
    #[must_use]
    pub fn brand_storage(mut self, storage: BoxedStorage) -> Self {
        self.brand_storage = Some(storage);
        self
    }

    /// Turn on per-tenant auth. Anonymous traffic goes to `/__login`,
    /// which checks credentials against `rustango_users` in the
    /// resolved tenant. Non-superusers get a read-only admin.
    ///
    /// You can share the `SessionSecret` with the operator console;
    /// the two use different cookie names.
    ///
    /// Without this the tenant admin has no auth at all, which suits
    /// demos and trusted intranets only.
    #[must_use]
    pub fn with_session(mut self, secret: tenant_console::SessionSecret) -> Self {
        let mut tera = Tera::default();
        // `tenant_login.html` includes `_theme_tokens.html`, so the
        // partial must be in the same Tera registry or the render
        // fails and the login page comes out blank.
        tera.add_raw_template(
            "_theme_tokens.html",
            include_str!("../styles/theme_tokens.html"),
        )
        .expect("_theme_tokens.html parses");
        tera.add_raw_template(
            "tenant_login.html",
            include_str!("templates/tenant_login.html"),
        )
        .expect("tenant_login.html parses");
        // v0.28.2 (#77) — self-serve change-password page.
        tera.add_raw_template(
            "tenant_change_password.html",
            include_str!("templates/tenant_change_password.html"),
        )
        .expect("tenant_change_password.html parses");
        self.session = Some(Arc::new(TenantSessionConfig { secret, tera }));
        self
    }

    /// Restrict the admin to these tables. Same semantics as
    /// `crate::admin::Builder::show_only`.
    #[must_use]
    pub fn show_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.show_only = Some(tables.into_iter().map(Into::into).collect());
        self
    }

    /// Mark these tables read-only. Same semantics as
    /// `crate::admin::Builder::read_only`.
    #[must_use]
    pub fn read_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.read_only.extend(tables.into_iter().map(Into::into));
        self
    }

    /// Register a user-defined bulk action handler. Same semantics as
    /// [`crate::admin::Builder::register_action`]. The handler runs on
    /// the resolved tenant's pool — search_path is already scoped to
    /// the tenant's schema.
    #[must_use]
    pub fn register_action<F>(
        mut self,
        model_table: &'static str,
        action_name: &'static str,
        handler: F,
    ) -> Self
    where
        F: for<'a> Fn(
                &'a crate::sql::Pool,
                &'a [crate::core::SqlValue],
            ) -> crate::admin::AdminActionFuture<'a>
            + Send
            + Sync
            + 'static,
    {
        self.actions.push(RegisteredAction {
            table: model_table,
            name: action_name,
            handler: Arc::new(handler),
        });
        self
    }
}

impl<DB: Database> TenantAdminBuilder<DB>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    /// Build the tenant-aware `axum::Router`. Catches every request
    /// via a fallback handler — mount it under whatever prefix you
    /// want via `Router::nest`.
    #[must_use]
    pub fn build(self) -> Router {
        let pools = self.pools;
        let registry_url = Arc::new(self.registry_url);
        let resolver = self.resolver;
        let show_only = Arc::new(self.show_only);
        let read_only = Arc::new(self.read_only);
        let session = self.session;
        let actions = Arc::new(self.actions);
        let title = Arc::new(self.title);
        let subtitle = Arc::new(self.subtitle);
        // Brand storage: explicit injection via `brand_storage(...)`,
        // or default to a `LocalStorage` rooted at
        // `RUSTANGO_BRAND_STORAGE_DIR` (default `./var/brand`). The
        // same backend serves both the operator console and the
        // tenant admin's `/__brand__/{slug}/{filename}` fallback.
        let brand_storage: BoxedStorage = self
            .brand_storage
            .unwrap_or_else(branding::default_brand_storage);
        let routes = self.routes;

        Router::new().fallback(move |req: Request<Body>| {
            let pools = pools.clone();
            let registry_url = registry_url.clone();
            let resolver = resolver.clone();
            let show_only = show_only.clone();
            let read_only = read_only.clone();
            let session = session.clone();
            let actions = actions.clone();
            let title = title.clone();
            let subtitle = subtitle.clone();
            let brand_storage = brand_storage.clone();
            let routes = routes.clone();
            async move {
                handle_request::<DB>(
                    req,
                    &pools,
                    &registry_url,
                    &*resolver,
                    &show_only,
                    &read_only,
                    session.as_deref(),
                    &actions,
                    title.as_deref().as_deref(),
                    subtitle.as_deref().as_deref(),
                    &brand_storage,
                    &routes,
                )
                .await
            }
        })
    }
}

async fn handle_request<DB: Database>(
    req: Request<Body>,
    pools: &TenantPools<DB>,
    registry_url: &str,
    resolver: &dyn OrgResolver,
    show_only: &Option<Vec<String>>,
    read_only: &[String],
    session: Option<&TenantSessionConfig>,
    actions: &[RegisteredAction],
    title: Option<&str>,
    subtitle: Option<&str>,
    brand_storage: &BoxedStorage,
    routes: &super::routes::RouteConfig,
) -> Response
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // v0.38 — `registry_url` is unused now that schema-mode pool
    // construction lives on `TenantPools<Postgres>::scoped_pool_dyn`.
    // Kept in the signature so the published `TenantAdminBuilder::new`
    // contract is unchanged.
    let _ = registry_url;
    // Public brand asset surface — `<brand_url>/{slug}/{filename}`.
    // Served before the resolver runs so the assets are reachable
    // even when the requesting host doesn't match a known tenant
    // (the slug in the path is validated by the branding module).
    let brand_prefix = format!("{}/", routes.brand_url);
    if let Some(rest) = req.uri().path().strip_prefix(&brand_prefix) {
        if let Some((slug, filename)) = rest.split_once('/') {
            return serve_brand_asset(slug, filename, brand_storage).await;
        }
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let (mut parts, body) = req.into_parts();
    let org = match resolver.resolve(&parts, &pools.registry_pool()).await {
        Ok(Some(o)) => o,
        Ok(None) => return (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(e) => {
            // Logged, not sent: the resolver error can name the registry host.
            let body = crate::error::server_error_body("tenancy::admin::resolve", &e);
            return (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
        }
    };

    // A schema-mode PG tenant gets a short-lived pool with
    // `search_path` already set; a database-mode tenant gets a cheap
    // clone of its cached pool. Schema mode on another backend errors.
    let pool = match pools.scoped_pool_dyn(&org).await {
        Ok(p) => p,
        Err(e) => {
            warn!(
                target: "rustango::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant pool build failed",
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Per-tenant auth is opt-in. Without `with_session`, every request
    // goes straight to the inner admin.
    let mut user_perms: Option<std::collections::HashSet<String>> = None;
    let mut session_user_id: Option<i64> = None;
    // Chrome session info threaded into the inner admin so the sidebar
    // renders "Signed in as <username>" + the Logout button.
    let mut session_username: Option<String> = None;
    let mut session_is_superuser = false;
    // Set when the operator console minted this session. Drives the
    // impersonation banner, and makes writes record
    // `operator:<id>:impersonating` in the audit log.
    let mut impersonated_by: Option<i64> = None;
    if let Some(cfg) = session {
        let path = parts.uri.path().to_owned();
        let method = parts.method.clone();

        // Public surface: login, logout, static assets. Paths come
        // from `RouteConfig`, so an app can drop the `__` prefix.
        let static_rustango_url = format!("{}/rustango.png", routes.static_url);
        if path == static_rustango_url {
            return rustango_png_response();
        }
        // Favicon. A square `icon.png`, not `.ico`: the shipped .ico
        // wraps a non-square image and renders poorly.
        let static_icon_png_url = format!("{}/icon.png", routes.static_url);
        if path == static_icon_png_url {
            return rustango_icon_png_response();
        }
        // Impersonation handoff. Redeem the console's signed token,
        // set a host-scoped cookie, and redirect to the admin index.
        // `JtiBlacklist` makes the token single use.
        if path == routes.impersonation_handoff_url && method == axum::http::Method::GET {
            return redeem_impersonation_handoff(&org, cfg, routes, parts.uri.query())
                .await
                .into_response();
        }
        // SSO (admin-sso) — multi-provider OpenID Connect / social OAuth.
        // `{login}/sso/{slug}` starts the handshake for one provider;
        // `{login}/sso/{slug}/callback` completes it. Both GET. The slug
        // resolves against the tenant's own `SsoProvider` rows first, then
        // the registry-wide `SharedSsoProvider` set.
        #[cfg(feature = "admin-sso")]
        if method == axum::http::Method::GET {
            let sso_prefix = format!("{}/sso/", routes.login_url);
            if let Some(rest) = path.strip_prefix(&sso_prefix) {
                let registry_pool = pools.registry_pool();
                if let Some(slug) = rest.strip_suffix("/callback") {
                    return super::sso::tenant_sso_callback(
                        &org,
                        slug,
                        &cfg.secret,
                        &pool,
                        &registry_pool,
                        routes,
                        &parts,
                    )
                    .await;
                }
                if !rest.is_empty() && !rest.contains('/') {
                    return super::sso::tenant_sso_begin(
                        rest,
                        &cfg.secret,
                        &pool,
                        &registry_pool,
                        routes,
                        &parts,
                    )
                    .await;
                }
            }
        }
        if path == routes.login_url {
            return match method {
                axum::http::Method::GET => {
                    #[cfg(feature = "admin-sso")]
                    let resp = {
                        let registry_pool = pools.registry_pool();
                        login_form(
                            &org,
                            cfg,
                            brand_storage,
                            routes,
                            parts.uri.query(),
                            &parts.headers,
                            &pool,
                            &registry_pool,
                        )
                        .await
                        .into_response()
                    };
                    #[cfg(not(feature = "admin-sso"))]
                    let resp = login_form(
                        &org,
                        cfg,
                        brand_storage,
                        routes,
                        parts.uri.query(),
                        &parts.headers,
                    )
                    .await
                    .into_response();
                    resp
                }
                axum::http::Method::POST => {
                    login_submit(&org, cfg, &pool, routes, parts.headers, body).await
                }
                _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
            };
        }
        if path == routes.logout_url && method == axum::http::Method::POST {
            use crate::signals::auth::{
                meta_from_headers, send_user_logged_out, UserLoggedOutContext,
            };
            // Best-effort: decode the session cookie so the signal
            // carries user_id / username. Receivers tolerate `None`.
            let (uid, uname) = decode_session_user(&parts.headers, cfg, &org.slug);
            let meta = meta_from_headers(&parts.headers, Some(routes.logout_url.as_str()));
            send_user_logged_out(UserLoggedOutContext {
                source: "tenant_admin",
                user_id: uid,
                username: uname,
                request: meta,
            })
            .await;
            return logout_response(routes);
        }
        // v0.27.8 (#78) — end-impersonation routes. Recognized
        // both with and without the configurable admin prefix
        // because the form action template emits the full path
        // (`{{ admin_prefix }}/__end-impersonation`) but a
        // direct API caller might POST to `/__end-impersonation`.
        let end_imp_full = format!("{}/__end-impersonation", routes.admin_url);
        if (path == end_imp_full || path == "/__end-impersonation")
            && method == axum::http::Method::POST
        {
            return end_impersonation_response(routes);
        }

        // Private surface — require a valid session cookie.
        match validate_session(&parts.headers, cfg, &org, &pool, &pools.registry_pool()).await {
            SessionCheck::Authenticated {
                is_superuser,
                user_id,
                username,
                impersonated_by: imp_by,
            } => {
                session_user_id = Some(user_id);
                session_username = Some(username);
                session_is_superuser = is_superuser;
                impersonated_by = imp_by;
                if !is_superuser {
                    // Fetch the user's effective codenames once per request
                    // and thread them into the inner admin builder so
                    // individual views can check add/change/delete/view perms
                    // per table without extra DB round-trips.
                    match super::permissions::user_permissions_pool(user_id, &pool).await {
                        Ok(codenames) => {
                            user_perms = Some(codenames.into_iter().collect());
                        }
                        Err(e) => {
                            warn!(
                                target: "rustango::tenancy::admin",
                                slug = %org.slug,
                                user_id,
                                error = %e,
                                "failed to fetch user permissions",
                            );
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "permission lookup failed",
                            )
                                .into_response();
                        }
                    }
                }
                // Superuser: user_perms stays None → all operations allowed.
            }
            SessionCheck::Anonymous => {
                return redirect_to_tenant_login(&path, routes).into_response();
            }
            SessionCheck::Error(msg) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response();
            }
        }

        // v0.28.2 (#77) — self-serve change-password page. Lives
        // outside `admin_url` so the prefix-strip logic below
        // doesn't rewrite the path before we get here. Requires
        // an authenticated session (handled above) — anonymous
        // visitors are bounced to the login page.
        if path == routes.change_password_url {
            let user_id = session_user_id.unwrap_or(0);
            return match parts.method {
                axum::http::Method::GET => {
                    change_password_form(&org, cfg, brand_storage, routes, parts.uri.query())
                        .into_response()
                }
                axum::http::Method::POST => {
                    change_password_submit(&org, &pool, routes, user_id, body).await
                }
                _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
            };
        }
    }

    let admin_router = build_inner_admin_router(
        pool.clone(),
        show_only,
        read_only,
        user_perms,
        actions,
        title,
        subtitle,
        &org,
        brand_storage,
        impersonated_by,
        routes.admin_url.as_str(),
        routes.change_password_url.as_str(),
        routes.logout_url.as_str(),
        routes.audit_url.as_str(),
        routes.static_url.as_str(),
    );

    // Strip the admin mount prefix so the inner router sees plain
    // `/{table}` paths. Login and logout go through the fallback and
    // carry no prefix.
    if let Some(stripped) = parts.uri.path().strip_prefix(routes.admin_url.as_str()) {
        let new_path = if stripped.is_empty() { "/" } else { stripped };
        let new_pq = if let Some(q) = parts.uri.query() {
            format!("{new_path}?{q}")
        } else {
            new_path.to_owned()
        };
        if let Ok(new_uri) = new_pq.parse::<axum::http::Uri>() {
            parts.uri = new_uri;
        }
    }

    let inner_req = Request::from_parts(parts, body);
    // Dispatch inside an `audit::with_source` scope so audited writes
    // pick up the signed-in user. With no session the source stays
    // `AuditSource::System`.
    let dispatch = async {
        match admin_router.oneshot(inner_req).await {
            Ok(r) => r,
            Err(_infallible) => unreachable!("axum::Router service is Infallible"),
        }
    };
    let audited = async {
        if let Some(uid) = session_user_id {
            crate::audit::with_source(
                crate::audit::AuditSource::User {
                    id: uid.to_string(),
                },
                dispatch,
            )
            .await
        } else {
            dispatch.await
        }
    };
    // Install the request's session into the admin task-local the inner
    // chrome reads (`admin::session::current()`), so the tenant admin
    // sidebar renders "Signed in as <username>" + the Logout button —
    // the bare admin gets this from its own `require_session` middleware,
    // which the tenant admin path bypasses.
    let response = match session_user_id {
        Some(uid) => {
            let sess = crate::admin::session::AdminSession {
                user_id: uid,
                username: session_username.clone().unwrap_or_default(),
                is_superuser: session_is_superuser,
            };
            crate::admin::session::CURRENT_SESSION
                .scope(sess, audited)
                .await
        }
        None => audited.await,
    };

    // Schema-mode pool is dropped here when `pool` falls out of
    // scope; database-mode pools are reference-counted and stay
    // cached.
    drop(pool);
    response
}

// ----------------------------- session helpers

enum SessionCheck {
    Authenticated {
        is_superuser: bool,
        /// Tenant-side `rustango_users.id` of the authenticated user.
        /// Threaded into `audit::with_source(User { id })` for the
        /// duration of the inner-router dispatch so any audited
        /// write picks up the user-attribution automatically.
        user_id: i64,
        /// Username of the authenticated user (empty for an
        /// operator-impersonation session, which has no tenant user).
        /// Threaded into the inner admin's chrome session so the
        /// sidebar renders "Signed in as <username>" + Logout.
        username: String,
        /// `Some(operator_id)` when this session was minted by
        /// the operator console's "Open admin as superuser →"
        /// flow (#78). Drives the impersonation banner +
        /// audit-log `source` shape.
        impersonated_by: Option<i64>,
    },
    Anonymous,
    Error(String),
}

/// Read the user id out of a session cookie, so the logout signal can
/// name the user without a database query.
///
/// The cookie holds only `uid`, so the username is always `None`; look
/// it up separately if you need it. Any decode problem — missing
/// cookie, bad signature, expired, wrong slug — gives `(None, None)`.
fn decode_session_user(
    headers: &HeaderMap,
    cfg: &TenantSessionConfig,
    slug: &str,
) -> (Option<i64>, Option<String>) {
    let Some(cookie_value) = read_cookie(headers, tenant_console::COOKIE_NAME) else {
        return (None, None);
    };
    match tenant_console::decode(&cfg.secret, slug, &cookie_value) {
        Ok(p) => (Some(p.uid), None),
        Err(_) => (None, None),
    }
}

async fn validate_session(
    headers: &HeaderMap,
    cfg: &TenantSessionConfig,
    org: &Org,
    tenant_pool: &crate::sql::Pool,
    registry_pool: &crate::sql::Pool,
) -> SessionCheck {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    let Some(cookie_value) = read_cookie(headers, tenant_console::COOKIE_NAME) else {
        return SessionCheck::Anonymous;
    };
    let payload = match tenant_console::decode(&cfg.secret, &org.slug, &cookie_value) {
        Ok(p) => p,
        Err(_) => return SessionCheck::Anonymous,
    };
    // An impersonation cookie from the operator console grants tenant
    // superuser. Re-check the operator still exists and is active on
    // every request: otherwise a deactivated operator keeps full admin
    // for the rest of the cookie's lifetime.
    if let Some(operator_id) = payload.imp {
        let ops: Vec<super::auth::Operator> = match super::auth::Operator::objects()
            .where_(super::auth::Operator::id.eq(operator_id))
            .fetch(registry_pool)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    target: "rustango::tenancy::admin",
                    operator_id,
                    error = %e,
                    "operator re-check failed during impersonation session validation",
                );
                return SessionCheck::Error("session lookup failed".into());
            }
        };
        return match ops.into_iter().next() {
            Some(op) if op.active => SessionCheck::Authenticated {
                is_superuser: true,
                user_id: 0,
                username: String::new(),
                impersonated_by: Some(operator_id),
            },
            // Operator gone or deactivated → drop the impersonation.
            _ => SessionCheck::Anonymous,
        };
    }
    // v0.38 — route through ORM `User::objects().fetch` so the
    // same body runs on PG / MySQL / SQLite. Identifier quoting +
    // placeholders are handled by the dialect emitter.
    let users: Vec<super::auth::User> = match super::auth::User::objects()
        .where_(super::auth::User::id.eq(payload.uid))
        .fetch(tenant_pool)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(
                target: "rustango::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant user lookup failed during session validation",
            );
            return SessionCheck::Error("session lookup failed".into());
        }
    };
    let Some(user) = users.into_iter().next() else {
        return SessionCheck::Anonymous;
    };
    if !user.active {
        return SessionCheck::Anonymous;
    }
    // v0.28.4 — invalidate sessions issued before the latest password
    // rotation. `password_changed_at IS NULL` means the account
    // predates v0.28.4 and never rotated; we don't enforce.
    if let Some(ts) = user.password_changed_at {
        if payload.iat < ts.timestamp() {
            return SessionCheck::Anonymous;
        }
    }
    SessionCheck::Authenticated {
        is_superuser: user.is_superuser,
        user_id: payload.uid,
        username: user.username.clone(),
        impersonated_by: None,
    }
}

fn redirect_to_tenant_login(next_path: &str, routes: &super::routes::RouteConfig) -> Redirect {
    let next = if next_path == routes.login_url || next_path.starts_with(&routes.logout_url) {
        "/".to_string()
    } else {
        next_path.to_string()
    };
    let location = format!("{}?next={}", routes.login_url, urlencoding_lite(&next));
    Redirect::to(&location)
}

#[allow(clippy::too_many_arguments)]
async fn login_form(
    org: &Org,
    cfg: &TenantSessionConfig,
    brand_storage: &BoxedStorage,
    routes: &super::routes::RouteConfig,
    query: Option<&str>,
    headers: &HeaderMap,
    #[cfg(feature = "admin-sso")] tenant_pool: &crate::sql::Pool,
    #[cfg(feature = "admin-sso")] registry_pool: &crate::sql::Pool,
) -> Response {
    let mut next: Option<String> = None;
    let mut error: Option<String> = None;
    if let Some(q) = query {
        for pair in q.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            let v = url_decode_lite(v);
            match k {
                "next" => next = Some(v),
                "error" => error = Some(v),
                _ => {}
            }
        }
    }
    let mut ctx = Context::new();
    ctx.insert("tenant_slug", &org.slug);
    ctx.insert("tenant_name", &org.display_name);
    ctx.insert("next", &next.unwrap_or_else(|| "/".into()));
    ctx.insert("error", &error);
    // Per-tenant brand, so even the login page shows the org's logo,
    // favicon, color, theme and name.
    let brand_name = org
        .brand_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&org.display_name);
    ctx.insert("brand_name", brand_name);
    ctx.insert("brand_tagline", &org.brand_tagline);
    let brand_logo_url =
        super::branding::brand_asset_url(&org.slug, org.logo_path.as_deref(), brand_storage);
    ctx.insert("brand_logo_url", &brand_logo_url);
    let brand_favicon_url =
        super::branding::brand_asset_url(&org.slug, org.favicon_path.as_deref(), brand_storage);
    ctx.insert("brand_favicon_url", &brand_favicon_url);
    let theme_mode = org
        .theme_mode
        .as_deref()
        .and_then(super::branding::validate_theme_mode)
        .unwrap_or("auto");
    ctx.insert("theme_mode", theme_mode);
    let brand_css = super::branding::build_brand_css(org);
    ctx.insert("brand_css", &brand_css);
    // v0.28.0 (#74) — login form action / static asset paths
    // come from RouteConfig so apps can flip to /login etc.
    ctx.insert("login_url", &routes.login_url);
    ctx.insert("static_url", &routes.static_url);
    // SSO (admin-sso) — one button per enabled provider (the tenant's own
    // `SsoProvider` rows merged with the registry-wide shared set).
    #[cfg(feature = "admin-sso")]
    {
        let providers = super::sso::list_enabled(tenant_pool, registry_pool, routes).await;
        ctx.insert("sso_enabled", &!providers.is_empty());
        ctx.insert("sso_providers", &providers);
    }
    // Seed the double-submit CSRF token so the first GET already
    // carries one; without it the first POST would always fail
    // (#1607). The cookie rides on this response.
    #[cfg(feature = "csrf")]
    let set_cookie = {
        let (token, cookie) =
            crate::forms::csrf::ensure_token(headers, crate::forms::csrf::CSRF_COOKIE);
        ctx.insert("csrf_token", &token);
        cookie
    };

    // v0.27.5 — log render errors instead of silently rendering an
    // empty body. The previous `unwrap_or_default()` hid a real
    // template-include resolution bug from the operator.
    let html = axum::response::Html(match cfg.tera.render("tenant_login.html", &ctx) {
        Ok(html) => html,
        Err(e) => {
            tracing::error!(
                target: "rustango::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant_login.html render failed",
            );
            "<!doctype html><html><body><h1>Login page unavailable</h1>\
             <p>The tenant login template failed to render. Check the \
             server logs for the underlying Tera error.</p></body></html>"
                .to_owned()
        }
    });

    let mut resp = html.into_response();
    #[cfg(feature = "csrf")]
    if let Some(cookie) = set_cookie {
        if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
    }
    resp
}

#[derive(serde::Deserialize)]
struct LoginSubmitForm {
    username: String,
    password: String,
    #[serde(default)]
    next: Option<String>,
    #[serde(default, rename = "_csrf")]
    csrf: Option<String>,
}

async fn login_submit(
    org: &Org,
    cfg: &TenantSessionConfig,
    tenant_pool: &crate::sql::Pool,
    routes: &super::routes::RouteConfig,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use crate::core::Column as _;
    use crate::signals::auth::{
        meta_from_headers, send_user_logged_in, send_user_login_failed, AuthFailureReason,
        UserLoggedInContext, UserLoginFailedContext,
    };
    use crate::sql::FetcherPool as _;

    let meta = meta_from_headers(&headers, Some(routes.login_url.as_str()));

    let bytes = match http_body_util::BodyExt::collect(body).await {
        Ok(b) => b.to_bytes(),
        Err(_) => return (StatusCode::BAD_REQUEST, "could not read body").into_response(),
    };
    let form: LoginSubmitForm = match serde_urlencoded::from_bytes(&bytes) {
        Ok(f) => f,
        Err(_) => return (StatusCode::BAD_REQUEST, "malformed login form").into_response(),
    };
    let next = sanitize_next(form.next.as_deref());

    // Login CSRF (#1607): reject before the user lookup, so a forged
    // POST costs nothing and cannot probe usernames by timing.
    // `SameSite=Lax` does not cover this — login CSRF sets a *new*
    // session rather than replaying an existing one, which is what
    // makes "the victim is now inside the attacker's account" possible.
    #[cfg(feature = "csrf")]
    if !crate::forms::csrf::verify_form_token(&headers, form.csrf.as_deref()) {
        return (StatusCode::FORBIDDEN, "CSRF token missing or mismatched").into_response();
    }

    // v0.38 — auth check via the tri-dialect ORM. The query targets
    // the tenant's `rustango_users` table on the user-supplied pool;
    // schema-mode PG handles search_path internally (the admin pool
    // is built with search_path baked in).
    let users: Vec<super::auth::User> = match super::auth::User::objects()
        .where_(super::auth::User::username.eq(form.username.clone()))
        .fetch(tenant_pool)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "rustango::tenancy::admin", error = %e, "login query");
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };
    let bad_creds = || -> Response {
        Redirect::to(&format!(
            "{}?error=Invalid+credentials&next={}",
            routes.login_url,
            urlencoding_lite(&next)
        ))
        .into_response()
    };
    let fire_failed = |reason: AuthFailureReason| {
        let ctx = UserLoginFailedContext {
            source: "tenant_admin",
            attempted_username: Some(form.username.clone()),
            reason,
            request: meta.clone(),
        };
        async move { send_user_login_failed(ctx).await }
    };
    let Some(user) = users.into_iter().next() else {
        // Audit H1 — this handler does its own lookup+verify (so it
        // wasn't covered by the authenticate_*_pool timing fix); spend a
        // verify's worth of work on the unknown-user path so timing
        // doesn't reveal whether the username exists.
        super::password::verify_dummy(&form.password);
        fire_failed(AuthFailureReason::InvalidCredentials).await;
        return bad_creds();
    };
    let uid: i64 = user.id.get().copied().unwrap_or(0);

    // Audit M1 (console) — per-account brute-force lockout, on by
    // default, keyed by tenant slug + resolved user id (no
    // arbitrary-name DoS, no cross-tenant id collision). A locked
    // account is rejected before the password verify.
    #[cfg(feature = "cache")]
    let lock_key = format!("tenant:{}:{}", org.slug, uid);
    #[cfg(feature = "cache")]
    if uid != 0 && crate::account_lockout::shared().is_locked(&lock_key).await {
        fire_failed(AuthFailureReason::InvalidCredentials).await;
        return bad_creds();
    }

    // Verify before the active check so active vs inactive accounts take
    // the same time (audit H1).
    let ok = matches!(
        super::password::verify(&form.password, &user.password_hash),
        Ok(true)
    );

    if !user.active || !ok || uid == 0 {
        // Audit M1 — count the failure against the resolved id (existing
        // accounts only).
        #[cfg(feature = "cache")]
        if uid != 0 {
            let _ = crate::account_lockout::shared()
                .record_failure(&lock_key)
                .await;
        }
        let reason = if !user.active {
            AuthFailureReason::Inactive
        } else {
            AuthFailureReason::InvalidCredentials
        };
        fire_failed(reason).await;
        return bad_creds();
    }

    // Audit M1 — successful login clears the failure counter + any lock.
    #[cfg(feature = "cache")]
    crate::account_lockout::shared().clear(&lock_key).await;
    let ttl_secs = i64::try_from(routes.tenant_session_ttl.as_secs())
        .unwrap_or(tenant_console::SESSION_TTL_SECS);
    let payload = TenantSessionPayload::new(uid, &org.slug, ttl_secs);
    let cookie_value = tenant_console::encode(&cfg.secret, &payload);
    let cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        // Secure in production; off in dev so plain-HTTP login works.
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(ttl_secs))
        .build();
    let mut resp = Redirect::to(&next).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("cookie is ascii"),
    );
    send_user_logged_in(UserLoggedInContext {
        source: "tenant_admin",
        user_id: uid,
        username: form.username.clone(),
        is_superuser: user.is_superuser,
        request: meta,
    })
    .await;
    resp
}

fn logout_response(routes: &super::routes::RouteConfig) -> Response {
    let clear = Cookie::build((tenant_console::COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        // Match the Secure flag used when setting it, or the browser
        // may not clear it.
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(0))
        .build();
    let mut resp = Redirect::to(&routes.login_url).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear.to_string()).expect("cookie is ascii"),
    );
    resp
}

/// Redeem an impersonation handoff token from the operator console.
/// Checks the signature, expiry, slug and single use, then sets the
/// host-scoped impersonation cookie and redirects to the admin index.
///
/// Every failure returns a plain 401. Detail here would help an
/// attacker probe the token format; the mint side logs the specifics.
async fn redeem_impersonation_handoff(
    org: &super::Org,
    cfg: &TenantSessionConfig,
    routes: &super::routes::RouteConfig,
    query: Option<&str>,
) -> Response {
    use super::impersonation_handoff::{decode, JtiBlacklist};

    let token = match query.and_then(extract_token_param) {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "missing token").into_response(),
    };
    let payload = match decode(&cfg.secret, &org.slug, &token) {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(
                target: "rustango::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "handoff token rejected",
            );
            return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
    };
    if let Err(e) = JtiBlacklist::shared()
        .mark_used(&payload.jti, payload.exp)
        .await
    {
        tracing::warn!(
            target: "rustango::tenancy::admin",
            slug = %org.slug,
            jti = %payload.jti,
            error = %e,
            "handoff token jti reuse rejected",
        );
        return (StatusCode::UNAUTHORIZED, "token already used").into_response();
    }

    // Token is good. Set the cookie host-scoped, with no `Domain=`,
    // so browsers accept it on localhost too.
    let ttl_secs = i64::try_from(routes.impersonation_ttl.as_secs())
        .unwrap_or(tenant_console::IMPERSONATION_TTL_SECS);
    let session = TenantSessionPayload::impersonation(payload.op, &org.slug, ttl_secs);
    let cookie_value = tenant_console::encode(&cfg.secret, &session);
    let cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        // Secure in production; off in dev so plain-HTTP login works.
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(ttl_secs))
        .build();

    // 302 to the admin index. Trailing slash so the path matches
    // the inner admin's "/" route under the configured prefix.
    let admin_path = routes.admin_url.trim_end_matches('/');
    let target = format!("{admin_path}/");
    let mut resp = Redirect::to(&target).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie.to_string()) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    // Keep the still-fresh URL out of `Referer`. The token is single
    // use, but a third-party script on the admin index would
    // otherwise see it in its access logs.
    resp.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    tracing::info!(
        target: "rustango::tenancy::admin",
        slug = %org.slug,
        operator_id = payload.op,
        ttl_secs,
        "redeemed impersonation handoff token",
    );
    resp
}

/// Pull the `token` value out of a raw query string. `token` is the
/// only parameter that matters here, so this avoids a parser
/// dependency.
fn extract_token_param(query: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            return Some(value.to_owned());
        }
    }
    None
}

/// Clear the impersonation cookie and send the browser back to the
/// operator console. The operator's own apex session cookie is left
/// alone, so they stay signed in there.
///
/// The apex URL comes from `RUSTANGO_APEX_DOMAIN`,
/// `RUSTANGO_TENANT_SCHEME` and `RUSTANGO_TENANT_PORT`. With none of
/// them set it falls back to `/`, which still clears the cookie.
fn end_impersonation_response(_routes: &super::routes::RouteConfig) -> Response {
    let clear = Cookie::build((tenant_console::COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        // Match the Secure flag used when setting it, or the browser
        // may not clear it.
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(0))
        .build();
    let scheme = std::env::var("RUSTANGO_TENANT_SCHEME").unwrap_or_else(|_| "http".into());
    let apex = std::env::var("RUSTANGO_APEX_DOMAIN").unwrap_or_else(|_| "localhost".into());
    let port_suffix = std::env::var("RUSTANGO_TENANT_PORT")
        .ok()
        .filter(|s| !s.is_empty() && s != "80" && s != "443")
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let target = format!("{scheme}://{apex}{port_suffix}/orgs");
    let mut resp = Redirect::to(&target).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear.to_string()).expect("cookie is ascii"),
    );
    resp
}

// ---------- v0.28.2 (#77) self-serve change password ----------

#[derive(serde::Deserialize)]
struct ChangePasswordSubmitForm {
    current_password: String,
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

fn change_password_form(
    org: &Org,
    cfg: &TenantSessionConfig,
    brand_storage: &BoxedStorage,
    routes: &super::routes::RouteConfig,
    query: Option<&str>,
) -> axum::response::Html<String> {
    let mut error: Option<String> = None;
    let mut success: Option<String> = None;
    if let Some(q) = query {
        for pair in q.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            let v = url_decode_lite(v);
            match k {
                "error" => error = Some(v),
                "ok" => success = Some(v),
                _ => {}
            }
        }
    }
    let mut ctx = Context::new();
    ctx.insert("tenant_slug", &org.slug);
    ctx.insert("tenant_name", &org.display_name);
    ctx.insert("error", &error);
    ctx.insert("success", &success);
    let brand_name = org
        .brand_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&org.display_name);
    ctx.insert("brand_name", brand_name);
    ctx.insert("brand_tagline", &org.brand_tagline);
    let brand_logo_url =
        super::branding::brand_asset_url(&org.slug, org.logo_path.as_deref(), brand_storage);
    ctx.insert("brand_logo_url", &brand_logo_url);
    let brand_favicon_url =
        super::branding::brand_asset_url(&org.slug, org.favicon_path.as_deref(), brand_storage);
    ctx.insert("brand_favicon_url", &brand_favicon_url);
    let theme_mode = org
        .theme_mode
        .as_deref()
        .and_then(super::branding::validate_theme_mode)
        .unwrap_or("auto");
    ctx.insert("theme_mode", theme_mode);
    let brand_css = super::branding::build_brand_css(org);
    ctx.insert("brand_css", &brand_css);
    ctx.insert("change_password_url", &routes.change_password_url);
    ctx.insert("admin_url", &routes.admin_url);
    ctx.insert("logout_url", &routes.logout_url);
    ctx.insert("static_url", &routes.static_url);
    axum::response::Html(match cfg.tera.render("tenant_change_password.html", &ctx) {
        Ok(html) => html,
        Err(e) => {
            tracing::error!(
                target: "rustango::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant_change_password.html render failed",
            );
            "<!doctype html><html><body><h1>Change-password page unavailable</h1>\
             <p>The tenant change-password template failed to render. Check the \
             server logs for the underlying Tera error.</p></body></html>"
                .to_owned()
        }
    })
}

async fn change_password_submit(
    _org: &Org,
    tenant_pool: &crate::sql::Pool,
    routes: &super::routes::RouteConfig,
    user_id: i64,
    body: Body,
) -> Response {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    let bytes = match http_body_util::BodyExt::collect(body).await {
        Ok(b) => b.to_bytes(),
        Err(_) => return (StatusCode::BAD_REQUEST, "could not read body").into_response(),
    };
    let form: ChangePasswordSubmitForm = match serde_urlencoded::from_bytes(&bytes) {
        Ok(f) => f,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "malformed change-password form").into_response()
        }
    };
    let redir_err = |msg: &str| -> Response {
        Redirect::to(&format!(
            "{}?error={}",
            routes.change_password_url,
            urlencoding_lite(msg)
        ))
        .into_response()
    };
    if form.current_password.is_empty() || form.new_password.is_empty() {
        return redir_err("All fields are required.");
    }
    if !form.confirm_password.is_empty() && form.confirm_password != form.new_password {
        return redir_err("New password and confirmation did not match.");
    }
    if form.new_password == form.current_password {
        return redir_err("New password must differ from the current password.");
    }
    if user_id <= 0 {
        return redir_err("Session is missing a user id; please log in again.");
    }
    // v0.38 — fetch the user via the tri-dialect ORM, verify current
    // password, then save the updated row. save_pool covers PG / MySQL
    // / SQLite and the dialect emitter handles the placeholder /
    // identifier quoting differences.
    let users: Vec<super::auth::User> = match super::auth::User::objects()
        .where_(super::auth::User::id.eq(user_id))
        .fetch(tenant_pool)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "rustango::tenancy::admin", error = %e, "change-password lookup");
            return (StatusCode::INTERNAL_SERVER_ERROR, "lookup failed").into_response();
        }
    };
    let Some(mut user) = users.into_iter().next() else {
        return redir_err("Your account no longer exists; please log in again.");
    };
    let ok = super::password::verify(&form.current_password, &user.password_hash).unwrap_or(false);
    if !ok {
        return redir_err("Current password did not match.");
    }
    let new_hash = match super::password::hash(&form.new_password) {
        Ok(h) => h,
        Err(e) => {
            return redir_err(&format!("hash failed: {e}"));
        }
    };
    user.password_hash = new_hash;
    user.password_changed_at = Some(chrono::Utc::now());
    if let Err(e) = user.save_pool(tenant_pool).await {
        warn!(target: "rustango::tenancy::admin", error = %e, "change-password update");
        return (StatusCode::INTERNAL_SERVER_ERROR, "update failed").into_response();
    }
    Redirect::to(&format!(
        "{}?ok=Password+updated",
        routes.change_password_url
    ))
    .into_response()
}

fn rustango_png_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from(tenant_console::RUSTANGO_PNG))
        .expect("response builds")
}

/// Serve the embedded square `icon.png` favicon at
/// `/__static__/icon.png`. v0.30.19. Cached aggressively (24h)
/// like the .png logo — both are framework-managed assets users
/// don't override per-tenant.
fn rustango_icon_png_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from(tenant_console::RUSTANGO_ICON_PNG))
        .expect("response builds")
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for piece in raw.split(';') {
        let piece = piece.trim();
        if let Some(value) = piece.strip_prefix(&format!("{name}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// Percent-decoder consolidated into [`crate::url_codec`] — same
// behavior the local `url_decode_lite` had (lossy UTF-8 conversion).
use crate::url_codec::url_decode as url_decode_lite;

fn sanitize_next(next: Option<&str>) -> String {
    sanitize_next_with_routes(next, &super::routes::RouteConfig::default())
}

fn sanitize_next_with_routes(next: Option<&str>, routes: &super::routes::RouteConfig) -> String {
    // `auth_decorators::safe_next` owns the shape check: it
    // percent-decodes, requires both the raw and decoded form to look
    // like a path, and rejects `/\`, which a browser rewrites into a
    // protocol-relative URL. Do not hand-roll it here.
    let Some(s) = next.and_then(crate::auth_decorators::safe_next) else {
        return "/".to_owned();
    };
    // Bouncing back to login/logout would loop. Not a safety check —
    // that is `safe_next`'s job — so it stays here.
    if s.starts_with(&routes.login_url) || s.starts_with(&routes.logout_url) {
        return "/".to_owned();
    }
    s
}

fn build_inner_admin_router(
    pool: crate::sql::Pool,
    show_only: &Option<Vec<String>>,
    read_only: &[String],
    user_perms: Option<std::collections::HashSet<String>>,
    actions: &[RegisteredAction],
    title: Option<&str>,
    subtitle: Option<&str>,
    org: &Org,
    brand_storage: &BoxedStorage,
    impersonated_by: Option<i64>,
    admin_url_prefix: &str,
    change_password_url: &str,
    logout_url: &str,
    audit_url: &str,
    static_url: &str,
) -> Router {
    // v0.27.7 — `tenant_mode()` filters registry-scoped models
    // (Org / Operator) out of the sidebar / index so the tenant
    // admin can't surface cross-tenant data. Standalone admins
    // (single-tenant projects using `crate::admin::Builder::new`
    // directly) leave the flag off and see every model.
    let mut builder = crate::admin::Builder::new(pool)
        .tenant_mode()
        // v0.28.0 (#74) — pass the configurable admin prefix
        // through to the inner admin's chrome_context so
        // template hrefs resolve correctly under any mount.
        .admin_prefix(admin_url_prefix)
        // Configurable audit suffix — drives both route
        // registration and template hrefs. Friendly default
        // (`/audit`) since v0.29 #85; legacy (`/__audit`)
        // available via `RouteConfig::legacy()`.
        .audit_url(audit_url)
        // v0.30.19 — pass the framework's static-asset URL so
        // admin templates can resolve the favicon link.
        .static_url(static_url)
        // v0.28.2 (#77) — surface the self-serve change-password
        // page in the sidebar.
        .change_password_url(change_password_url)
        // Sidebar Logout posts here (the tenancy-layer logout route),
        // not the bare admin's `{prefix}/logout` which isn't mounted.
        .logout_url(logout_url);
    // v0.27.8 (#78) — propagate the impersonation flag so chrome
    // renders the banner + audit-log emit picks it up.
    if let Some(operator_id) = impersonated_by {
        builder = builder.impersonated_by(operator_id);
    }
    if let Some(allow) = show_only {
        builder = builder.show_only(allow.iter().cloned());
    }
    if !read_only.is_empty() {
        builder = builder.read_only(read_only.iter().cloned());
    }
    if let Some(perms) = user_perms {
        builder = builder.with_user_perms(perms);
    }
    if let Some(t) = title {
        builder = builder.title(t);
    }
    if let Some(s) = subtitle {
        builder = builder.subtitle(s);
    }

    // Per-tenant branding overrides the static title/subtitle when
    // set on the resolved Org. Fall through to `display_name` as a
    // last resort so the sidebar always names the current tenant.
    if let Some(name) = org.brand_name.as_deref() {
        builder = builder.brand_name(name);
    } else if !org.display_name.is_empty() {
        builder = builder.brand_name(&org.display_name);
    }
    if let Some(tag) = org.brand_tagline.as_deref() {
        builder = builder.brand_tagline(tag);
    }
    if let Some(logo_url) =
        branding::brand_asset_url(&org.slug, org.logo_path.as_deref(), brand_storage)
    {
        builder = builder.brand_logo_url(logo_url);
    }
    if let Some(mode) = org
        .theme_mode
        .as_deref()
        .and_then(branding::validate_theme_mode)
    {
        builder = builder.theme_mode(mode);
    }
    if let Some(css) = branding::build_brand_css(org) {
        builder = builder.tenant_brand_css(css);
    }

    for action in actions {
        let handler = action.handler.clone();
        builder = builder.register_action(action.table, action.name, move |pool, pks| {
            handler(pool, pks)
        });
    }
    builder.build()
}

/// Serve a per-tenant brand asset from the shared brand storage.
/// The slug + filename are validated by the branding module — any
/// path-traversal attempt comes back as a 404.
async fn serve_brand_asset(slug: &str, filename: &str, brand_storage: &BoxedStorage) -> Response {
    match branding::load_brand_asset(slug, filename, brand_storage).await {
        Ok((bytes, ct)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, ct)
            .header(header::CACHE_CONTROL, "public, max-age=300")
            .body(Body::from(bytes))
            .expect("response builds")
            .into_response(),
        Err(
            branding::BrandError::NotFound
            | branding::BrandError::InvalidSlug
            | branding::BrandError::InvalidFilename,
        ) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            warn!(target: "rustango::tenancy::admin", error = %e, "brand asset");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// #1526. Exercises `sanitize_next_with_routes` — the function
/// `login_submit` actually calls at `:974`, whose result reaches
/// `Location` — rather than `urls::url_has_allowed_host_and_scheme`,
/// which nothing in this codebase calls and which is where the
/// original fix landed.
#[cfg(test)]
mod sanitize_next_tests {
    use super::sanitize_next_with_routes;
    use crate::tenancy::routes::RouteConfig;

    fn nxt(s: Option<&str>) -> String {
        sanitize_next_with_routes(s, &RouteConfig::default())
    }

    #[test]
    fn a_backslash_the_browser_rewrites_is_refused() {
        // Each leaves as protocol-relative `//evil…` once the browser
        // applies its `\` → `/` rewrite, while starting with `/` in
        // the source text — which is what the old hand-rolled check
        // accepted.
        for hostile in [
            "/\\evil.example/x",
            "/\\\\evil.example/x",
            "\\/evil.example/x",
            "/%5Cevil.example/x",
        ] {
            assert_eq!(
                nxt(Some(hostile)),
                "/",
                "`{hostile}` must not reach Location"
            );
        }
    }

    #[test]
    fn the_classic_shapes_are_still_refused() {
        for hostile in [
            "//evil.example/x",
            "https://evil.example",
            "javascript:1",
            // The browser strips TAB, CR and LF while parsing a URL
            // (WHATWG URL 4.1), so each of these leaves as the
            // protocol-relative `//evil.example` (#1604 security-001).
            "/\x09/evil.example/x",
            "/\x0d/evil.example/x",
            "/\x0a/evil.example/x",
        ] {
            assert_eq!(nxt(Some(hostile)), "/", "{hostile}");
        }
        assert_eq!(nxt(None), "/");
    }

    #[test]
    fn an_ordinary_path_still_survives() {
        // The control. Without it, a sanitizer that returned "/" for
        // everything would satisfy both assertions above and quietly
        // break every post-login redirect.
        for ok in ["/admin/posts", "/admin/posts?page=2"] {
            assert_eq!(nxt(Some(ok)), ok, "{ok} should survive");
        }
    }

    #[test]
    fn the_login_loop_guard_still_applies() {
        // Not a safety check — it stops a redirect loop — but it is
        // the behaviour the old hand-rolled version also provided, so
        // delegating must not drop it.
        let routes = RouteConfig::default();
        assert_eq!(
            sanitize_next_with_routes(Some(&routes.login_url), &routes),
            "/",
        );
    }
}
