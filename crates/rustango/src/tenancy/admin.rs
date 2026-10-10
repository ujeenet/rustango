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
    session: Option<TenantSessionConfig>,
    jti_store: Option<Arc<dyn crate::jti_store::JtiStore>>,
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
    perm: crate::admin::ActionPerm,
    handler: crate::admin::AdminActionFn,
}

struct TenantSessionConfig {
    secret: tenant_console::SessionSecret,
    tera: Tera,
    /// Used handoff tokens and ended impersonations; `None` is the
    /// per-process [`JtiBlacklist::shared`] (#2176).
    ///
    /// [`JtiBlacklist::shared`]: super::impersonation_handoff::JtiBlacklist::shared
    jti: Option<super::impersonation_handoff::JtiBlacklist>,
}

impl TenantSessionConfig {
    fn jti(&self) -> &super::impersonation_handoff::JtiBlacklist {
        self.jti
            .as_ref()
            .unwrap_or_else(|| super::impersonation_handoff::JtiBlacklist::shared())
    }
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
            jti_store: None,
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
        let mut tera = crate::template_extensions::html_tera();
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
        self.session = Some(TenantSessionConfig {
            secret,
            tera,
            jti: None,
        });
        self
    }

    /// Where used impersonation handoff tokens and ended impersonations
    /// are kept. The default is per-process; with several replicas pass
    /// a shared store such as Redis or the database (#2176). Call it before
    /// or after [`Self::with_session`]; without a session it has no use.
    #[must_use]
    pub fn impersonation_jti_store(mut self, store: Arc<dyn crate::jti_store::JtiStore>) -> Self {
        self.jti_store = Some(store);
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
        self,
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
        self.register_action_with_perm(
            model_table,
            action_name,
            crate::admin::ActionPerm::Change,
            handler,
        )
    }

    /// [`Self::register_action`] checked against `perm` (#1818). Same
    /// semantics as [`crate::admin::Builder::register_action_with_perm`].
    #[must_use]
    pub fn register_action_with_perm<F>(
        mut self,
        model_table: &'static str,
        action_name: &'static str,
        perm: crate::admin::ActionPerm,
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
            perm,
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
        let jti_store = self.jti_store;
        if jti_store.is_some() && self.session.is_none() {
            warn!(
                target: "rustango::tenancy::admin",
                "impersonation_jti_store is unused: the tenant admin has no with_session",
            );
        }
        let session = self.session.map(|mut s| {
            s.jti = jti_store.map(super::impersonation_handoff::JtiBlacklist::with_store);
            Arc::new(s)
        });
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

        // CSRF on every tenant admin write (#1713): tenant hosts are
        // same-site, so `SameSite=Lax` lets `t02` post to `t01`. Same
        // pair as the console: the layer checks token and Origin, and
        // inside it `csrf_context` sets the token every form renders.
        // `layer`, not `route_layer`: everything here is the fallback.
        let router = Router::new().fallback(move |req: Request<Body>| {
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
        });
        router
            .layer(axum::middleware::from_fn(
                crate::admin::csrf_context::csrf_context,
            ))
            .layer(crate::forms::csrf::layer())
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

    // A path-prefix tenant's admin lives under its prefix (#2059).
    // Cookies are scoped the same way, so they keep apart too (#2098).
    let cookie_path = super::routes::cookie_path(&org, parts.uri.path());
    let prefixed;
    let routes = if cookie_path == "/" {
        routes
    } else {
        prefixed = routes.under_prefix(cookie_path);
        &prefixed
    };

    // A schema-mode PG tenant gets a short-lived pool with
    // `search_path` already set; a database-mode tenant gets a cheap
    // clone of its cached pool. Schema mode on another backend errors.
    let pool = match pools.scoped_pool_dyn(&org).await {
        Ok(p) => p,
        Err(e) => {
            // Before the session check, and the text can name the tenant DB host.
            let body = crate::error::server_error_body("tenancy::admin::pool", &e);
            return (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
        }
    };

    // Per-tenant auth is opt-in. Without `with_session`, every request
    // goes straight to the inner admin.
    let mut user_perms: Option<std::collections::HashSet<String>> = None;
    // Who the session acts as: the chrome, the audit source and the
    // impersonation banner all come from it.
    let mut actor: Option<SessionActor> = None;
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
            return redeem_impersonation_handoff(
                &org,
                cfg,
                routes,
                cookie_path,
                parts.uri.query(),
                &pools.registry_pool(),
            )
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
                        cookie_path,
                        &parts,
                    )
                    .await;
                }
                if !rest.is_empty() && !rest.contains('/') {
                    return super::sso::tenant_sso_begin(
                        &org,
                        rest,
                        &cfg.secret,
                        &pool,
                        &registry_pool,
                        routes,
                        cookie_path,
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
                            &parts.extensions,
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
                        &parts.extensions,
                    )
                    .await
                    .into_response();
                    resp
                }
                axum::http::Method::POST => {
                    login_submit(
                        &org,
                        cfg,
                        &pool,
                        routes,
                        cookie_path,
                        &parts.extensions,
                        parts.headers,
                        body,
                    )
                    .await
                }
                _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
            };
        }
        if path == routes.logout_url && method == axum::http::Method::POST {
            use crate::signals::auth::{
                meta_from_parts, send_user_logged_out, UserLoggedOutContext,
            };
            // Best-effort: decode the session cookie so the signal
            // carries user_id / username. Receivers tolerate `None`.
            let (uid, uname) = decode_session_user(&parts.headers, cfg, &org.slug);
            let meta = meta_from_parts(
                &parts.extensions,
                &parts.headers,
                Some(routes.logout_url.as_str()),
            );
            // End the user's sessions everywhere. An impersonation logout
            // ends only that impersonation.
            end_impersonation_session(&parts.headers, cfg, &org.slug).await;
            let revoked =
                match validate_session(&parts.headers, cfg, &org, &pool, &pools.registry_pool())
                    .await
                {
                    SessionCheck::Authenticated {
                        actor:
                            SessionActor::User {
                                id,
                                sessions_revoked_at,
                                ..
                            },
                        iat,
                    } => crate::session::revoke_sessions::<super::auth::User>(
                        &pool,
                        id,
                        sessions_revoked_at,
                        iat,
                    )
                    .await
                    .map(drop)
                    .map_err(|e| e.to_string()),
                    // Never report a logout that did not happen.
                    SessionCheck::Error(e) => Err(e),
                    _ => Ok(()),
                };
            if let Err(e) = revoked {
                warn!(target: "rustango::tenancy::admin", slug = %org.slug, error = %e, "logout revoke failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "logout failed").into_response();
            }
            send_user_logged_out(UserLoggedOutContext {
                source: "tenant_admin",
                user_id: uid,
                username: uname,
                request: meta,
            })
            .await;
            return logout_response(routes, cookie_path);
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
            end_impersonation_session(&parts.headers, cfg, &org.slug).await;
            return end_impersonation_response(cookie_path);
        }

        // Private surface — require a valid session cookie.
        match validate_session(&parts.headers, cfg, &org, &pool, &pools.registry_pool()).await {
            SessionCheck::Authenticated { actor: who, .. } => {
                let regular_user = match &who {
                    SessionActor::User {
                        id,
                        is_superuser: false,
                        ..
                    } => Some(*id),
                    _ => None,
                };
                actor = Some(who);
                if let Some(user_id) = regular_user {
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
            // An impersonation has no tenant user; 0 matches no row.
            let user_id = match &actor {
                Some(SessionActor::User { id, .. }) => *id,
                _ => 0,
            };
            return match parts.method {
                axum::http::Method::GET => {
                    change_password_form(&org, cfg, brand_storage, routes, parts.uri.query())
                        .into_response()
                }
                axum::http::Method::POST => {
                    let ip = crate::login_throttle::ClientIp::from_parts(
                        &parts.extensions,
                        &parts.headers,
                    );
                    change_password_submit(&org, &pool, routes, user_id, &ip, body).await
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
        actor.as_ref().and_then(SessionActor::impersonated_by),
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

    // Same two places the bare admin's `require_session` puts it:
    // request extensions (handlers, custom views) and the task-local
    // (chrome).
    let admin_session = actor.as_ref().map(SessionActor::admin_session);
    if let Some(sess) = &admin_session {
        parts.extensions.insert(sess.clone());
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
        if let Some(who) = &actor {
            // Bound to this tenant, so work this request hands off does
            // not stamp it on other tenants' rows.
            crate::audit::with_tenant_source(who.audit_source(), org.slug.clone(), dispatch).await
        } else {
            dispatch.await
        }
    };
    let response = match admin_session {
        Some(sess) => {
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
        actor: SessionActor,
        /// The cookie's issued-at, so logout's cut-off covers it (#1855).
        iat: i64,
    },
    Anonymous,
    Error(String),
}

/// Who a live session acts as. An impersonating operator is named by id
/// only: a tenant user can take any username (#2110).
#[derive(Debug)]
enum SessionActor {
    User {
        /// This tenant's `rustango_users.id`.
        id: i64,
        username: String,
        is_superuser: bool,
        /// The user's cut-off, for logout (#1855).
        sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// An operator from the console's "Open admin as superuser" (#78).
    Operator { id: i64 },
}

impl SessionActor {
    fn impersonated_by(&self) -> Option<i64> {
        match self {
            Self::User { .. } => None,
            Self::Operator { id } => Some(*id),
        }
    }

    /// The `source` audited writes record; the same as `updated_by`.
    fn audit_source(&self) -> crate::audit::AuditSource {
        self.admin_session().actor()
    }

    fn admin_session(&self) -> crate::admin::session::AdminSession {
        match self {
            Self::User {
                id,
                username,
                is_superuser,
                ..
            } => crate::admin::session::AdminSession::new(*id, username.clone(), *is_superuser),
            Self::Operator { id } => crate::admin::session::AdminSession::impersonation(*id),
        }
    }
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
    // superuser. Re-check on every request that the operator still
    // exists, is active and has the same password.
    if let Some(operator_id) = payload.imp {
        // No `sid` means a pre-#2038 cookie that no logout can revoke.
        let Some(sid) = payload.sid.as_deref() else {
            return SessionCheck::Anonymous;
        };
        if cfg.jti().session_ended(sid).await {
            return SessionCheck::Anonymous;
        }
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
            Some(op)
                if op.active
                    && super::session::session_survives(
                        &cfg.secret,
                        &payload.pwf,
                        payload.iat,
                        &op.password_hash,
                        op.password_changed_at,
                        op.sessions_revoked_at,
                    ) =>
            {
                SessionCheck::Authenticated {
                    actor: SessionActor::Operator { id: operator_id },
                    iat: payload.iat,
                }
            }
            // Operator gone, deactivated, changed password or logged out → drop it.
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
    // Invalidate sessions minted before the latest password change or logout.
    if !super::session::session_survives(
        &cfg.secret,
        &payload.pwf,
        payload.iat,
        &user.password_hash,
        user.password_changed_at,
        user.sessions_revoked_at,
    ) {
        return SessionCheck::Anonymous;
    }
    SessionCheck::Authenticated {
        actor: SessionActor::User {
            id: payload.uid,
            username: user.username,
            is_superuser: user.is_superuser,
            sessions_revoked_at: user.sessions_revoked_at,
        },
        iat: payload.iat,
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
    extensions: &axum::http::Extensions,
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
    // Under `build()` the request's token (and its cookie) comes from
    // `csrf_context`; minting a second one here would not match (#1713).
    #[cfg(feature = "csrf")]
    let set_cookie = match crate::admin::session::current_csrf_token() {
        Some(token) => {
            ctx.insert("csrf_token", &token);
            None
        }
        None => {
            let (token, cookie) = crate::forms::csrf::ensure_token_under_layer(headers, extensions);
            ctx.insert("csrf_token", &token);
            cookie
        }
    };

    // v0.27.5 — log render errors instead of silently rendering an
    // empty body. The previous `unwrap_or_default()` hid a real
    // template-include resolution bug from the operator.
    ctx.insert(
        "csp_nonce",
        &crate::csp_nonce::current().unwrap_or_default(),
    );
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
    cookie_path: &str,
    extensions: &axum::http::Extensions,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use crate::core::Column as _;
    use crate::login_throttle::LoginRefused;
    use crate::signals::auth::{
        meta_from_parts, send_user_logged_in, send_user_login_failed, AuthFailureReason,
        UserLoggedInContext, UserLoginFailedContext,
    };
    use crate::sql::FetcherPool as _;

    let ip = crate::login_throttle::ClientIp::from_parts(extensions, &headers);
    let meta = meta_from_parts(extensions, &headers, Some(routes.login_url.as_str()));

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
    if !crate::forms::csrf::verify_form_token_under_layer(
        &headers,
        extensions,
        form.csrf.as_deref(),
    ) {
        return (StatusCode::FORBIDDEN, "CSRF token missing or mismatched").into_response();
    }

    // Rate limits and the account lock, before the lookup (#1609).
    let mut attempt = match crate::login_throttle::shared()
        .begin(
            &crate::login_throttle::LoginScope::Tenant(org.slug.clone()),
            &ip,
            &form.username,
        )
        .await
    {
        Ok(a) => a,
        Err(refused) => return refused.into_response(),
    };

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
    let Some(mut user) = users.into_iter().next() else {
        // Audit H1 — this handler does its own lookup+verify (so it
        // wasn't covered by the authenticate_*_pool timing fix); spend a
        // verify's worth of work on the unknown-user path so timing
        // doesn't reveal whether the username exists.
        if super::password::verify_dummy_async(&form.password)
            .await
            .is_err()
        {
            return LoginRefused::Busy.into_response();
        }
        attempt.failed().await;
        fire_failed(AuthFailureReason::InvalidCredentials).await;
        return bad_creds();
    };
    if let Err(refused) = attempt.resolve(&user.username).await {
        return refused.into_response();
    }
    let uid: i64 = user.id.get().copied().unwrap_or(0);

    // Verify before the active check so active vs inactive accounts take
    // the same time (audit H1).
    let ok = match super::password::verify_async(&form.password, &user.password_hash).await {
        Ok(ok) => ok,
        Err(super::TenancyError::Busy) => return LoginRefused::Busy.into_response(),
        Err(_) => false,
    };

    if !user.active || !ok || uid == 0 {
        attempt.failed().await;
        let reason = if !user.active {
            AuthFailureReason::Inactive
        } else {
            AuthFailureReason::InvalidCredentials
        };
        fire_failed(reason).await;
        return bad_creds();
    }

    attempt.succeeded().await;
    user.password_hash = crate::passwords::upgrade_stored_hash(
        tenant_pool,
        <super::auth::User as crate::core::Model>::SCHEMA,
        uid,
        &form.password,
        &user.password_hash,
    )
    .await;
    let ttl_secs = i64::try_from(routes.tenant_session_ttl.as_secs())
        .unwrap_or(tenant_console::SESSION_TTL_SECS);
    let mut payload = TenantSessionPayload::new(
        uid,
        &org.slug,
        ttl_secs,
        super::session::PasswordFingerprint::of(&cfg.secret, &user.password_hash),
    );
    payload.iat = crate::session::issued_at(user.sessions_revoked_at);
    let cookie_value = tenant_console::encode(&cfg.secret, &payload);
    let cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path(cookie_path.to_owned())
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

fn logout_response(routes: &super::routes::RouteConfig, cookie_path: &str) -> Response {
    let clear = Cookie::build((tenant_console::COOKIE_NAME, ""))
        .path(cookie_path.to_owned())
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
    cookie_path: &str,
    query: Option<&str>,
    registry: &crate::sql::Pool,
) -> Response {
    use super::impersonation_handoff::decode;
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

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
    // A token minted before the operator logged out or changed password is dead.
    let op = match super::auth::Operator::objects()
        .where_(super::auth::Operator::id.eq(payload.op))
        .fetch(registry)
        .await
    {
        Ok(rows) => rows.into_iter().next(),
        Err(e) => {
            warn!(target: "rustango::tenancy::admin", slug = %org.slug, error = %e, "handoff operator lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "operator lookup failed").into_response();
        }
    };
    if !op.is_some_and(|op| {
        op.active
            && super::session::session_survives(
                &cfg.secret,
                &payload.pwf,
                payload.iat,
                &op.password_hash,
                op.password_changed_at,
                op.sessions_revoked_at,
            )
    }) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    if let Err(e) = cfg.jti().mark_used(&payload.jti, payload.exp).await {
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
    let mut session = TenantSessionPayload::impersonation(
        payload.op,
        &org.slug,
        ttl_secs,
        payload.pwf,
        payload.jti,
    );
    // The session dates from the handoff's mint, on the console's clock.
    session.iat = payload.iat;
    let cookie_value = tenant_console::encode(&cfg.secret, &session);
    let cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path(cookie_path.to_owned())
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

/// Revoke the request's impersonation cookie, if it is one; the
/// operator stays signed in to the console (#2038).
async fn end_impersonation_session(headers: &HeaderMap, cfg: &TenantSessionConfig, slug: &str) {
    let Some(value) = read_cookie(headers, tenant_console::COOKIE_NAME) else {
        return;
    };
    if let Ok(TenantSessionPayload {
        imp: Some(_),
        sid: Some(sid),
        exp,
        ..
    }) = tenant_console::decode(&cfg.secret, slug, &value)
    {
        cfg.jti().end_session(&sid, exp).await;
    }
}

/// Clear the impersonation cookie and send the browser back to the
/// operator console. The operator's own apex session cookie is left
/// alone, so they stay signed in there.
///
/// The apex URL comes from `RUSTANGO_APEX_DOMAIN`,
/// `RUSTANGO_TENANT_SCHEME` and `RUSTANGO_TENANT_PORT`. With none of
/// them set it falls back to `/`, which still clears the cookie.
fn end_impersonation_response(cookie_path: &str) -> Response {
    // Also the legacy `Path=/` one, from before the cookie was path-scoped.
    let mut paths = vec![cookie_path];
    if cookie_path != "/" {
        paths.push("/");
    }
    let clears: Vec<String> = paths
        .into_iter()
        .map(|path| {
            Cookie::build((tenant_console::COOKIE_NAME, ""))
                .path(path.to_owned())
                .http_only(true)
                .same_site(SameSite::Lax)
                // Match the Secure flag used when setting it, or the browser
                // may not clear it.
                .secure(crate::session::secure_cookies())
                .max_age(CookieDuration::seconds(0))
                .build()
                .to_string()
        })
        .collect();
    let apex = crate::tenancy::server::apex_domain();
    let port_suffix = std::env::var("RUSTANGO_TENANT_PORT")
        .ok()
        .filter(|s| !s.is_empty() && s != "80" && s != "443")
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let target = format!(
        "{}/orgs",
        crate::tenancy::server::tenant_origin(&apex, &port_suffix)
    );
    let mut resp = Redirect::to(&target).into_response();
    for clear in clears {
        resp.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&clear).expect("cookie is ascii"),
        );
    }
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
    if let Some(token) = crate::admin::session::current_csrf_token() {
        ctx.insert("csrf_token", &token);
    }
    ctx.insert(
        "csp_nonce",
        &crate::csp_nonce::current().unwrap_or_default(),
    );
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
    org: &Org,
    tenant_pool: &crate::sql::Pool,
    routes: &super::routes::RouteConfig,
    user_id: i64,
    ip: &crate::login_throttle::ClientIp,
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
    if let Err(e) = crate::password_validators::check_builtin_form_password(&form.new_password) {
        return redir_err(&e.message);
    }
    if user_id <= 0 {
        return redir_err("Session is missing a user id; please log in again.");
    }
    // Verify the current password, then write only the password columns.
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
    let Some(user) = users.into_iter().next() else {
        return redir_err("Your account no longer exists; please log in again.");
    };
    let verify = async {
        match super::password::verify_async(&form.current_password, &user.password_hash).await {
            Ok(ok) => Ok(ok),
            Err(super::TenancyError::Busy) => Err(crate::login_throttle::LoginRefused::Busy),
            Err(_) => Ok(false),
        }
    };
    let ok = match crate::login_throttle::shared()
        .verify_current_password(
            &crate::login_throttle::LoginScope::Tenant(org.slug.clone()),
            ip,
            &user.username,
            verify,
        )
        .await
    {
        Ok(ok) => ok,
        Err(refused) => return refused.into_response(),
    };
    if !ok {
        return redir_err("Current password did not match.");
    }
    let new_hash = match super::password::hash_async(&form.new_password).await {
        Ok(h) => h,
        Err(super::TenancyError::Busy) => {
            return crate::login_throttle::LoginRefused::Busy.into_response()
        }
        Err(e) => {
            // The hasher's text stays in the log, not the redirect URL (#2021).
            warn!(target: "rustango::tenancy::admin", error = %e, "change-password hash");
            return redir_err("Could not update the password; please try again.");
        }
    };
    match crate::passwords::store_password_change(
        tenant_pool,
        <super::auth::User as crate::core::Model>::SCHEMA,
        &user.id,
        &user.password_hash,
        &new_hash,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return redir_err("Your password changed meanwhile; please try again."),
        Err(e) => {
            warn!(target: "rustango::tenancy::admin", error = %e, "change-password update");
            return (StatusCode::INTERNAL_SERVER_ERROR, "update failed").into_response();
        }
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
    crate::cookies::cookie_from_headers(headers, name).map(str::to_owned)
}

// The crate's one query-value encoder (#1663); it also escapes `/`.
use crate::url_codec::url_encode as urlencoding_lite;

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
    // admin can't surface cross-tenant data.
    let mut builder = crate::admin::Builder::new(pool)
        .tenant_mode()
        // The tenancy session layer gates every route of this admin.
        .gated_upstream()
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
        builder = builder.register_action_with_perm(
            action.table,
            action.name,
            action.perm,
            move |pool, pks| handler(pool, pks),
        );
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
        // Unauthenticated route; a storage error can name the bucket or path.
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::server_error_body("tenancy::admin::brand_asset", &e),
        )
            .into_response(),
    }
}

/// #1526. Exercises `sanitize_next_with_routes` — the function
/// `login_submit` actually calls at `:974`, whose result reaches
/// `Location` — rather than `urls::url_has_allowed_host_and_scheme`,
/// which nothing in this codebase calls and which is where the
/// original fix landed.
/// A resolver failure is a 500 whose body withholds the driver text (#1684).
#[cfg(all(test, feature = "sqlite"))]
mod resolver_error_tests {
    use super::*;
    use crate::tenancy::TenancyError;

    struct Failing;

    #[async_trait::async_trait]
    impl OrgResolver for Failing {
        async fn resolve(
            &self,
            _parts: &axum::http::request::Parts,
            _registry: &crate::sql::Pool,
        ) -> Result<Option<Org>, TenancyError> {
            Err(TenancyError::Resolution(
                "could not reach registry-db:5432".into(),
            ))
        }
    }

    #[tokio::test]
    async fn the_500_withholds_the_resolver_text() {
        let _env = crate::error::test_env::lock();
        let registry = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let pools = TenantPools::<sqlx::Sqlite>::new(registry);
        let storage: BoxedStorage = Arc::new(crate::storage::InMemoryStorage::new());
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = handle_request(
            req,
            &pools,
            "",
            &Failing,
            &None,
            &[],
            None,
            &[],
            None,
            None,
            &storage,
            &super::super::routes::RouteConfig::default(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("registry-db"), "{text}");
    }
}

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

/// #1627 — the tenant admin is gated by the tenancy session layer, so
/// building it must not raise `check --deploy`'s ungated warning.
#[cfg(all(test, feature = "sqlite"))]
mod ungated_flag_tests {
    #[tokio::test]
    async fn the_tenant_admin_is_not_flagged_ungated() {
        let _g = crate::admin::ungated_flag_lock().lock().await;
        crate::admin::reset_ungated_admin_built();
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let storage: super::BoxedStorage =
            std::sync::Arc::new(crate::storage::InMemoryStorage::new());
        let _admin = super::build_inner_admin_router(
            pool,
            &None,
            &[],
            None,
            &[],
            None,
            None,
            &crate::testkit::org(),
            &storage,
            None,
            "/admin",
            "/change-password",
            "/logout",
            "/audit",
            "/static",
        );
        let flagged = crate::admin::ungated_admin_built();
        crate::admin::reset_ungated_admin_built();
        assert!(!flagged, "the tenancy-gated admin was flagged ungated");
    }
}
