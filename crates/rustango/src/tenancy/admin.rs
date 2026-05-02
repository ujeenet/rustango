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

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Router;
use cookie::time::Duration as CookieDuration;
use cookie::{Cookie, SameSite};
use crate::sql::sqlx::postgres::{PgPool, PgPoolOptions};
use crate::sql::sqlx::Row;
use tera::{Context, Tera};
use tower::ServiceExt;
use tracing::warn;

use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::pools::TenantPools;
use super::resolver::OrgResolver;
use super::tenant_console::{self, TenantSessionPayload};

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
        }
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
        tera.add_raw_template(
            "tenant_login.html",
            include_str!("templates/tenant_login.html"),
        )
        .expect("tenant_login.html parses");
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
) -> Response {
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
    if let Some(cfg) = session {
        let path = parts.uri.path().to_owned();
        let method = parts.method.clone();

        // Public surface — `/__login*`, `/__logout`, `/__static__/rustango.png`.
        // These bypass the session check.
        if path == "/__static__/rustango.png" {
            return rustango_png_response();
        }
        if path == "/__login" {
            return match method {
                axum::http::Method::GET => {
                    login_form(&org, cfg, parts.uri.query()).into_response()
                }
                axum::http::Method::POST => {
                    login_submit(&org, cfg, pool.pg_pool(), parts.headers, body).await
                }
                _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
            };
        }
        if path == "/__logout" && method == axum::http::Method::POST {
            return logout_response();
        }

        // Private surface — require a valid session cookie.
        match validate_session(&parts.headers, cfg, &org, pool.pg_pool()).await {
            SessionCheck::Authenticated {
                is_superuser,
                user_id,
            } => {
                session_user_id = Some(user_id);
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
                            return (StatusCode::INTERNAL_SERVER_ERROR, "permission lookup failed")
                                .into_response();
                        }
                    }
                }
                // Superuser: user_perms stays None → all operations allowed.
            }
            SessionCheck::Anonymous => {
                return redirect_to_tenant_login(&path).into_response();
            }
            SessionCheck::Error(msg) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response();
            }
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
    );

    // Strip the `/__admin` mount prefix from the request URI so the
    // inner admin router sees plain `/{table}` paths. Requests routed
    // via the explicit `/__admin/{*rest}` route in the builder carry the
    // full URI; session-only paths (`/__login`, `/__logout`) go through
    // the fallback and are NOT prefixed.
    if let Some(stripped) = parts.uri.path().strip_prefix("/__admin") {
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
    // Look up the user in the tenant's storage; this gives us a
    // fresh `is_superuser` and `active` flag (operator can toggle
    // either mid-session). The query mirrors `auth::authenticate_user`
    // but without the password verify.
    match rustango::sql::sqlx::query(
        "SELECT is_superuser, active FROM rustango_users WHERE id = $1",
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
            let is_superuser: bool = row.try_get("is_superuser").unwrap_or(false);
            SessionCheck::Authenticated {
                is_superuser,
                user_id: payload.uid,
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

fn redirect_to_tenant_login(next_path: &str) -> Redirect {
    let next = if next_path == "/__login" || next_path.starts_with("/__logout") {
        "/".to_string()
    } else {
        next_path.to_string()
    };
    let location = format!("/__login?next={}", urlencoding_lite(&next));
    Redirect::to(&location)
}

fn login_form(org: &Org, cfg: &TenantSessionConfig, query: Option<&str>) -> axum::response::Html<String> {
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
    axum::response::Html(cfg.tera.render("tenant_login.html", &ctx).unwrap_or_default())
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
            "/__login?error=Invalid+credentials&next={}",
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
    let payload = TenantSessionPayload::new(uid, &org.slug, tenant_console::SESSION_TTL_SECS);
    let cookie_value = tenant_console::encode(&cfg.secret, &payload);
    let cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(tenant_console::SESSION_TTL_SECS))
        .build();
    let mut resp = Redirect::to(&next).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("cookie is ascii"),
    );
    resp
}

fn logout_response() -> Response {
    let clear = Cookie::build((tenant_console::COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(0))
        .build();
    let mut resp = Redirect::to("/__login").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear.to_string()).expect("cookie is ascii"),
    );
    resp
}

fn rustango_png_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from(tenant_console::RUSTANGO_PNG))
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

fn url_decode_lite(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(((hi << 4) | lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sanitize_next(next: Option<&str>) -> String {
    match next {
        Some(s)
            if s.starts_with('/')
                && !s.starts_with("//")
                && !s.contains("://")
                && !s.starts_with("/__login")
                && !s.starts_with("/__logout") =>
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
                super::pools::TenantPool::Schema { .. } => unreachable!(
                    "StorageMode::Database parsed but pool_for_org returned Schema"
                ),
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
                let stmt = format!(
                    "SET search_path TO {}, public",
                    quote_ident(&schema)
                );
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
) -> Router {
    let mut builder = crate::admin::Builder::new(pool);
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
    for action in actions {
        let handler = action.handler.clone();
        builder = builder.register_action(action.table, action.name, move |pool, pks| {
            handler(pool, pks)
        });
    }
    builder.build()
}

fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
