//! Tenant-aware admin — wraps `rustango-admin` with per-request
//! resolver dispatch.
//!
//! The headline UX (after Slice 6 lands per-tenant auth):
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
//! Per-request flow:
//!
//! 1. Resolver runs against `request.parts + registry`.
//! 2. `Ok(None)` → 404.
//! 3. `Ok(Some(org))` →
//!    * **Database mode**: clones the tenant's cached `PgPool` and
//!      builds a one-shot `rustango-admin` router with it.
//!    * **Schema mode**: spins up a *short-lived* `PgPool` with an
//!      `after_connect` hook setting `search_path` so admin queries
//!      hit the tenant's schema. Dropped after the request.
//! 4. The inner router's response is returned verbatim.
//!
//! ## Costs
//!
//! Per request:
//! * 1 SQL lookup for resolver (`Org` row). v0.6+ will likely add a
//!   small TTL cache — none in slice 4.
//! * Database-mode: 0 extra connections; cached pool re-used.
//! * Schema-mode: 1+ Postgres connections per request (the
//!   short-lived pool's `after_connect` runs `SET search_path` on
//!   every fresh connection it opens; sqlx may reuse them within
//!   the request). Real cost; v0.6 may switch to a connection-level
//!   model that avoids the per-request pool build.
//! * 1 small allocator hit for the inner Router construction.
//!
//! ## Per-tenant auth (v0.6 step 7)
//!
//! Opt-in via `TenantAdminBuilder::with_session(SessionSecret)`:
//!
//! * Anon traffic redirected to `/__login` (303).
//! * `POST /__login` calls `auth::authenticate_user` against the
//!   resolved tenant's pool; on success issues a signed cookie.
//! * `is_superuser = true` tenants get full read/write admin.
//! * `is_superuser = false` tenants get a `read_only_all` admin —
//!   list/detail render but every mutating route 403s and
//!   write-buttons are hidden.
//!
//! Operator UI bypass at the apex remains the caller's
//! responsibility — compose via host-based dispatch (see
//! `multitenant_demo`).

use std::sync::Arc;

use crate::sql::sqlx::postgres::{PgPool, PgPoolOptions};
use crate::sql::sqlx::Row;
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
use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::pools::TenantPools;
use super::resolver::OrgResolver;
use super::tenant_console::{self, TenantSessionPayload};
use crate::storage::BoxedStorage;

/// Builder for the tenant-aware admin router.
pub struct TenantAdminBuilder {
    pools: Arc<TenantPools>,
    registry_url: String,
    resolver: Arc<dyn OrgResolver>,
    show_only: Option<Vec<String>>,
    read_only: Vec<String>,
    session: Option<Arc<TenantSessionConfig>>,
    actions: Vec<RegisteredAction>,
    title: Option<String>,
    subtitle: Option<String>,
    brand_storage: Option<BoxedStorage>,
    /// v0.28.0 (#74) — configurable URL prefixes. Defaults to
    /// `RouteConfig::default()` (legacy `__`-prefixed paths) so
    /// upgrade is a no-op until apps set `routes(...)`.
    routes: Arc<super::routes::RouteConfig>,
}

/// One row in the action registry threaded through the tenant admin
/// builder. Re-applied per request when the inner admin router is
/// constructed for the resolved tenant.
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

impl TenantAdminBuilder {
    /// Build a tenant-aware admin handler.
    ///
    /// `registry_url` is the connection string used to spin up
    /// short-lived schema-mode admin pools. Database-mode tenants
    /// don't need it (their pool comes from `TenantPools`); pass
    /// any valid URL if you only have database-mode tenants.
    #[must_use]
    pub fn new(
        pools: Arc<TenantPools>,
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
        }
    }

    /// Override the default URL prefixes (#74, v0.28.0). When
    /// the operator console is also part of your app, pass the
    /// same `RouteConfig` to `operator_console::router_*` so
    /// both sides agree on `/login`, `/admin`, etc.
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

    /// Override the storage backend used for per-tenant brand assets
    /// (logo / favicon). Accepts any [`BoxedStorage`] — `LocalStorage`,
    /// `S3Storage` (AWS / R2 / B2 / MinIO), `InMemoryStorage` for
    /// tests, or any user-supplied `Storage` impl. When the backend
    /// exposes URLs via `Storage::url`, rendered `<img src>` tags
    /// point straight at the origin/CDN — no proxy through this
    /// process. When `None` is configured (the default), the
    /// framework falls back to
    /// [`super::branding::default_brand_storage`] (a `LocalStorage`
    /// rooted at `./var/brand` or `RUSTANGO_BRAND_STORAGE_DIR`).
    #[must_use]
    pub fn brand_storage(mut self, storage: BoxedStorage) -> Self {
        self.brand_storage = Some(storage);
        self
    }

    /// Enable per-tenant auth. Anon traffic gets redirected to
    /// `/__login`; `POST /__login` verifies credentials against
    /// `rustango_users` in the resolved tenant; non-superusers see
    /// a read-only admin (mutations 403). The same `SessionSecret`
    /// can be shared with the operator console — different cookie
    /// names keep the two domains isolated.
    ///
    /// Without this opt-in, the tenant admin remains unauthenticated
    /// (the v0.5 behavior — useful for demos and trusted intranet
    /// deployments).
    #[must_use]
    pub fn with_session(mut self, secret: tenant_console::SessionSecret) -> Self {
        let mut tera = Tera::default();
        // v0.27.5 — `tenant_login.html` includes `_theme_tokens.html`
        // (added in 0.27.3 #71 for the brand-on-login page). The
        // include must be registered in the same Tera registry or
        // Tera fails to resolve it and `render` returns an error,
        // which `login_form` swallows via `unwrap_or_default()` —
        // the operator sees a blank page. Adding the partial here
        // is the minimal fix.
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
                &'a crate::sql::sqlx::PgPool,
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
                handle_request(
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

async fn handle_request(
    req: Request<Body>,
    pools: &TenantPools,
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
) -> Response {
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
    let org = match resolver.resolve(&parts, pools.registry()).await {
        Ok(Some(o)) => o,
        Ok(None) => return (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(e) => {
            warn!(target: "crate::tenancy::admin", error = %e, "resolver error");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let pool = match build_admin_pool_for_tenant(&org, pools, registry_url).await {
        Ok(p) => p,
        Err(e) => {
            warn!(
                target: "crate::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant pool build failed",
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Per-tenant auth opt-in. Without `with_session`, the v0.5 path
    // still applies — every request goes straight to the inner admin.
    let mut user_perms: Option<std::collections::HashSet<String>> = None;
    let mut session_user_id: Option<i64> = None;
    // v0.27.8 (#78) — populated when the session was minted by
    // the operator console's impersonation flow. Threaded into
    // the chrome context for the impersonation banner + into
    // audit-log emit so writes record `operator:<id>:impersonating`.
    let mut impersonated_by: Option<i64> = None;
    if let Some(cfg) = session {
        let path = parts.uri.path().to_owned();
        let method = parts.method.clone();

        // Public surface — login / logout / brand-static. v0.28.0
        // (#74): paths come from `RouteConfig` so apps can map
        // `/login` etc. without the `__` prefix.
        let static_rustango_url = format!("{}/rustango.png", routes.static_url);
        if path == static_rustango_url {
            return rustango_png_response();
        }
        // v0.30.19 — favicon route. Square `icon.png` chosen over
        // `.ico` because the .ico file the brand assets shipped
        // wraps a non-square inner image and renders poorly across
        // browsers.
        let static_icon_png_url = format!("{}/icon.png", routes.static_url);
        if path == static_icon_png_url {
            return rustango_icon_png_response();
        }
        // v0.29 (#88) — operator-as-superuser impersonation
        // handoff. The operator console mints a signed
        // `HandoffPayload` and 302s the browser here; we redeem
        // the token, set a host-scoped impersonation cookie, and
        // 302 onward to the admin index. Single-use enforced via
        // `JtiBlacklist` so a leaked URL can't be replayed.
        if path == routes.impersonation_handoff_url && method == axum::http::Method::GET {
            return redeem_impersonation_handoff(&org, cfg, routes, parts.uri.query())
                .into_response();
        }
        if path == routes.login_url {
            return match method {
                axum::http::Method::GET => {
                    login_form(&org, cfg, brand_storage, routes, parts.uri.query()).into_response()
                }
                axum::http::Method::POST => {
                    login_submit(&org, cfg, pool.pg_pool(), routes, parts.headers, body).await
                }
                _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
            };
        }
        if path == routes.logout_url && method == axum::http::Method::POST {
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
        match validate_session(&parts.headers, cfg, &org, pool.pg_pool()).await {
            SessionCheck::Authenticated {
                is_superuser,
                user_id,
                impersonated_by: imp_by,
            } => {
                session_user_id = Some(user_id);
                impersonated_by = imp_by;
                if !is_superuser {
                    // Fetch the user's effective codenames once per request
                    // and thread them into the inner admin builder so
                    // individual views can check add/change/delete/view perms
                    // per table without extra DB round-trips.
                    match super::permissions::user_permissions(user_id, pool.pg_pool()).await {
                        Ok(codenames) => {
                            user_perms = Some(codenames.into_iter().collect());
                        }
                        Err(e) => {
                            warn!(
                                target: "crate::tenancy::admin",
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
                    change_password_submit(&org, pool.pg_pool(), routes, user_id, body).await
                }
                _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
            };
        }
    }

    let admin_router = build_inner_admin_router(
        pool.pg_pool().clone(),
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
        routes.audit_url.as_str(),
        routes.static_url.as_str(),
    );

    // Strip the configurable admin mount prefix from the request
    // URI so the inner admin router sees plain `/{table}` paths.
    // Requests routed via the explicit `<admin_url>/{*rest}`
    // route in the builder carry the full URI; session-only
    // paths (login / logout) go through the fallback and are
    // NOT prefixed. (#74, v0.28.0)
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
    // v0.12.1: wrap the inner-router dispatch in an `audit::with_source`
    // scope so any audited Model write inside the request picks up
    // the authenticated user automatically. Anonymous public surface
    // and projects without `with_session` get `AuditSource::System`
    // by default (no scope entered).
    let dispatch = async {
        match admin_router.oneshot(inner_req).await {
            Ok(r) => r,
            Err(_infallible) => unreachable!("axum::Router service is Infallible"),
        }
    };
    let response = if let Some(uid) = session_user_id {
        crate::audit::with_source(
            crate::audit::AuditSource::User {
                id: uid.to_string(),
            },
            dispatch,
        )
        .await
    } else {
        dispatch.await
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
        /// `Some(operator_id)` when this session was minted by
        /// the operator console's "Open admin as superuser →"
        /// flow (#78). Drives the impersonation banner +
        /// audit-log `source` shape.
        impersonated_by: Option<i64>,
    },
    Anonymous,
    Error(String),
}

async fn validate_session(
    headers: &HeaderMap,
    cfg: &TenantSessionConfig,
    org: &Org,
    tenant_pool: &PgPool,
) -> SessionCheck {
    let Some(cookie_value) = read_cookie(headers, tenant_console::COOKIE_NAME) else {
        return SessionCheck::Anonymous;
    };
    let payload = match tenant_console::decode(&cfg.secret, &org.slug, &cookie_value) {
        Ok(p) => p,
        Err(_) => return SessionCheck::Anonymous,
    };
    // v0.27.8 (#78) — impersonation cookies are minted by the
    // operator console; the tenant admin trusts them as
    // superuser without consulting `rustango_users`. The
    // operator id is preserved in `impersonated_by` so the
    // banner + audit-log source pick it up.
    if let Some(operator_id) = payload.imp {
        return SessionCheck::Authenticated {
            is_superuser: true,
            user_id: 0,
            impersonated_by: Some(operator_id),
        };
    }
    // Look up the user in the tenant's storage; this gives us a
    // fresh `is_superuser` and `active` flag (operator can toggle
    // either mid-session). The query mirrors `auth::authenticate_user`
    // but without the password verify. v0.28.4 (#77) — also fetches
    // `password_changed_at` so a session minted before the latest
    // password rotation is rejected.
    match rustango::sql::sqlx::query(
        "SELECT is_superuser, active, password_changed_at \
         FROM rustango_users WHERE id = $1",
    )
    .bind(payload.uid)
    .fetch_optional(tenant_pool)
    .await
    {
        Ok(Some(row)) => {
            let active: bool = row.try_get("active").unwrap_or(false);
            if !active {
                return SessionCheck::Anonymous;
            }
            // v0.28.4 — invalidate sessions issued before the latest
            // password rotation. `password_changed_at IS NULL` means
            // the account predates v0.28.4 and never rotated, so
            // we don't enforce.
            let pwd_changed: Option<chrono::DateTime<chrono::Utc>> =
                row.try_get("password_changed_at").ok().flatten();
            if let Some(ts) = pwd_changed {
                if payload.iat < ts.timestamp() {
                    return SessionCheck::Anonymous;
                }
            }
            let is_superuser: bool = row.try_get("is_superuser").unwrap_or(false);
            SessionCheck::Authenticated {
                is_superuser,
                user_id: payload.uid,
                impersonated_by: None,
            }
        }
        Ok(None) => SessionCheck::Anonymous,
        Err(e) => {
            warn!(
                target: "crate::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant user lookup failed during session validation",
            );
            SessionCheck::Error("session lookup failed".into())
        }
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

fn login_form(
    org: &Org,
    cfg: &TenantSessionConfig,
    brand_storage: &BoxedStorage,
    routes: &super::routes::RouteConfig,
    query: Option<&str>,
) -> axum::response::Html<String> {
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
    // v0.27.3 (#71) — thread per-tenant brand context so the
    // unauthenticated login page picks up the org's logo,
    // favicon, brand color, theme, and display name. Pre-fix
    // these fields were absent from the context and the template
    // hardcoded `/__static__/rustango.png` + `--accent: #2c6fb0`,
    // so uploaded brand assets never reached the login screen.
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
    // v0.27.5 — log render errors instead of silently rendering an
    // empty body. The previous `unwrap_or_default()` hid a real
    // template-include resolution bug from the operator.
    axum::response::Html(match cfg.tera.render("tenant_login.html", &ctx) {
        Ok(html) => html,
        Err(e) => {
            tracing::error!(
                target: "crate::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant_login.html render failed",
            );
            "<!doctype html><html><body><h1>Login page unavailable</h1>\
             <p>The tenant login template failed to render. Check the \
             server logs for the underlying Tera error.</p></body></html>"
                .to_owned()
        }
    })
}

#[derive(serde::Deserialize)]
struct LoginSubmitForm {
    username: String,
    password: String,
    #[serde(default)]
    next: Option<String>,
}

async fn login_submit(
    org: &Org,
    cfg: &TenantSessionConfig,
    tenant_pool: &PgPool,
    routes: &super::routes::RouteConfig,
    _headers: HeaderMap,
    body: Body,
) -> Response {
    let bytes = match http_body_util::BodyExt::collect(body).await {
        Ok(b) => b.to_bytes(),
        Err(_) => return (StatusCode::BAD_REQUEST, "could not read body").into_response(),
    };
    let form: LoginSubmitForm = match serde_urlencoded::from_bytes(&bytes) {
        Ok(f) => f,
        Err(_) => return (StatusCode::BAD_REQUEST, "malformed login form").into_response(),
    };
    let next = sanitize_next(form.next.as_deref());

    // Hand-write the auth check against the tenant pool. We can't
    // call `auth::authenticate_user` because that takes a
    // `&mut PgConnection`; here we have a `&PgPool` and want to run
    // a single query.
    let row = match rustango::sql::sqlx::query(
        "SELECT id, password_hash, is_superuser, active FROM rustango_users \
         WHERE username = $1",
    )
    .bind(&form.username)
    .fetch_optional(tenant_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "crate::tenancy::admin", error = %e, "login query");
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
    let Some(row) = row else {
        return bad_creds();
    };
    let active: bool = row.try_get("active").unwrap_or(false);
    if !active {
        return bad_creds();
    }
    let hash: String = match row.try_get("password_hash") {
        Ok(h) => h,
        Err(_) => return bad_creds(),
    };
    let ok = match super::password::verify(&form.password, &hash) {
        Ok(b) => b,
        Err(_) => false,
    };
    if !ok {
        return bad_creds();
    }
    let uid: i64 = match row.try_get("id") {
        Ok(v) => v,
        Err(_) => return bad_creds(),
    };
    let ttl_secs = i64::try_from(routes.tenant_session_ttl.as_secs())
        .unwrap_or(tenant_console::SESSION_TTL_SECS);
    let payload = TenantSessionPayload::new(uid, &org.slug, ttl_secs);
    let cookie_value = tenant_console::encode(&cfg.secret, &payload);
    let cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(ttl_secs))
        .build();
    let mut resp = Redirect::to(&next).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("cookie is ascii"),
    );
    resp
}

fn logout_response(routes: &super::routes::RouteConfig) -> Response {
    let clear = Cookie::build((tenant_console::COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(0))
        .build();
    let mut resp = Redirect::to(&routes.login_url).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear.to_string()).expect("cookie is ascii"),
    );
    resp
}

/// v0.29 (#88) — redeem an operator-console-minted impersonation
/// handoff token. Validates signature + expiry + slug binding +
/// single-use, then mints the host-scoped impersonation cookie
/// and 302s onward to the admin index.
///
/// All failure modes return a 401 with a generic body — the
/// operator console emits descriptive logs on the mint side, and
/// leaking detail here would help an attacker tune a brute-force
/// against the token format.
fn redeem_impersonation_handoff(
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
                target: "crate::tenancy::admin",
                slug = %org.slug,
                error = %e,
                "handoff token rejected",
            );
            return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
    };
    if let Err(e) = JtiBlacklist::shared().mark_used(&payload.jti, payload.exp) {
        tracing::warn!(
            target: "crate::tenancy::admin",
            slug = %org.slug,
            jti = %payload.jti,
            error = %e,
            "handoff token jti reuse rejected",
        );
        return (StatusCode::UNAUTHORIZED, "token already used").into_response();
    }

    // Valid token — mint the host-scoped impersonation cookie.
    // No `Domain=` so Chromium accepts it on localhost.
    let ttl_secs = i64::try_from(routes.impersonation_ttl.as_secs())
        .unwrap_or(tenant_console::IMPERSONATION_TTL_SECS);
    let session = TenantSessionPayload::impersonation(payload.op, &org.slug, ttl_secs);
    let cookie_value = tenant_console::encode(&cfg.secret, &session);
    let cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
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
    // Belt-and-suspenders: prevent the destination page from
    // leaking the still-fresh URL via Referer to anything it
    // loads. The token is single-use anyway, but third-party
    // scripts on the admin index would otherwise see the
    // redeemed token in their access logs.
    resp.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    tracing::info!(
        target: "crate::tenancy::admin",
        slug = %org.slug,
        operator_id = payload.op,
        ttl_secs,
        "redeemed impersonation handoff token",
    );
    resp
}

/// Pull the `token` value out of a raw query string. Skips a
/// dependency on `serde_urlencoded` — the only param we care
/// about is `token`, and any extra params ride along harmlessly.
fn extract_token_param(query: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            return Some(value.to_owned());
        }
    }
    None
}

/// v0.27.8 (#78) — clear the impersonation cookie and 302
/// back to the operator console at the apex. The operator's
/// own session cookie (`rustango_op_session`) on the apex
/// hostname is unaffected so they don't have to re-login.
///
/// Apex URL is read from `RUSTANGO_APEX_DOMAIN` /
/// `RUSTANGO_TENANT_SCHEME` / `RUSTANGO_TENANT_PORT`. Falls
/// back to `/` (relative) when the env vars aren't set, which
/// at least clears the cookie cleanly even if it sends the
/// browser somewhere unexpected.
fn end_impersonation_response(_routes: &super::routes::RouteConfig) -> Response {
    let clear = Cookie::build((tenant_console::COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
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
                target: "crate::tenancy::admin",
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
    tenant_pool: &PgPool,
    routes: &super::routes::RouteConfig,
    user_id: i64,
    body: Body,
) -> Response {
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
    let stored: Option<String> = match rustango::sql::sqlx::query_scalar(
        "SELECT password_hash FROM rustango_users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(tenant_pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "crate::tenancy::admin", error = %e, "change-password lookup");
            return (StatusCode::INTERNAL_SERVER_ERROR, "lookup failed").into_response();
        }
    };
    let Some(stored_hash) = stored else {
        return redir_err("Your account no longer exists; please log in again.");
    };
    let ok = super::password::verify(&form.current_password, &stored_hash).unwrap_or(false);
    if !ok {
        return redir_err("Current password did not match.");
    }
    let new_hash = match super::password::hash(&form.new_password) {
        Ok(h) => h,
        Err(e) => {
            return redir_err(&format!("hash failed: {e}"));
        }
    };
    if let Err(e) = rustango::sql::sqlx::query(
        "UPDATE rustango_users SET password_hash = $1, password_changed_at = NOW() WHERE id = $2",
    )
    .bind(&new_hash)
    .bind(user_id)
    .execute(tenant_pool)
    .await
    {
        warn!(target: "crate::tenancy::admin", error = %e, "change-password update");
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
    match next {
        Some(s)
            if s.starts_with('/')
                && !s.starts_with("//")
                && !s.contains("://")
                && !s.starts_with(&routes.login_url)
                && !s.starts_with(&routes.logout_url) =>
        {
            s.to_owned()
        }
        _ => "/".to_owned(),
    }
}

/// Wrapper around the tenant's PgPool that owns the schema-mode
/// short-lived pool's lifetime; for database-mode it just holds an
/// `Arc<PgPool>`.
enum AdminPool {
    /// Cached database-mode pool — cheap clone of an Arc.
    Database(Arc<PgPool>),
    /// Short-lived schema-mode pool — closed when dropped.
    Schema(PgPool),
}

impl AdminPool {
    fn pg_pool(&self) -> &PgPool {
        match self {
            Self::Database(p) => p,
            Self::Schema(p) => p,
        }
    }
}

impl Drop for AdminPool {
    fn drop(&mut self) {
        // For schema-mode we'd ideally `pool.close().await` — but
        // Drop can't be async. sqlx's PgPool background reaper will
        // eventually close idle connections; not ideal but
        // acceptable for slice 4. v0.6 may move to a per-request
        // connection (no pool) to avoid this entirely.
    }
}

async fn build_admin_pool_for_tenant(
    org: &Org,
    pools: &TenantPools,
    registry_url: &str,
) -> Result<AdminPool, TenancyError> {
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{}` has unknown storage_mode `{got}`",
            org.slug
        ))
    })?;
    match mode {
        StorageMode::Database => {
            let tp = pools.pool_for_org(org).await?;
            match tp {
                super::pools::TenantPool::Database { pool } => Ok(AdminPool::Database(pool)),
                super::pools::TenantPool::Schema { .. } => {
                    unreachable!("StorageMode::Database parsed but pool_for_org returned Schema")
                }
            }
        }
        StorageMode::Schema => {
            let schema = org.schema_name.clone().unwrap_or_else(|| org.slug.clone());
            let pool = build_short_lived_schema_pool(registry_url, &schema).await?;
            Ok(AdminPool::Schema(pool))
        }
    }
}

/// Build a short-lived `PgPool` whose every connection has its
/// `search_path` set to `<schema>, public`. Used for one admin
/// request, then dropped. Mirrors the migration helper in
/// [`crate::migrate`] but with a smaller pool size — admin
/// requests typically issue 1-3 queries.
async fn build_short_lived_schema_pool(
    registry_url: &str,
    schema: &str,
) -> Result<PgPool, TenancyError> {
    let schema_owned: Arc<str> = Arc::from(schema);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            let schema = Arc::clone(&schema_owned);
            Box::pin(async move {
                let stmt = format!("SET search_path TO {}, public", quote_ident(&schema));
                rustango::sql::sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect(registry_url)
        .await?;
    Ok(pool)
}

fn build_inner_admin_router(
    pool: PgPool,
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
        .change_password_url(change_password_url);
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
            warn!(target: "crate::tenancy::admin", error = %e, "brand asset");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
