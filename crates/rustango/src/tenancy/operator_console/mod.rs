//! Operator-facing admin console — replaces `protect_with_basic_auth`
//! for operator routes with form-based login + a sidebar layout.
//!
//! Independent from `rustango-admin` so the operator UX can evolve
//! without touching the per-tenant admin look. Wired into the demo
//! at the apex (`localhost:8080`); production deployments mount it
//! the same way.
//!
//! ## What it ships
//!
//! * `GET  /login`               — form HTML
//! * `POST /login`               — verify credentials, set cookie, redirect
//! * `POST /logout`              — clear cookie
//! * `GET  /`                    — welcome page (rustango image + intro)
//! * `GET  /operators`           — list of operators (read-only)
//! * `GET  /orgs`                — list of orgs (read-only)
//! * `GET  /orgs/{slug}/edit`    — edit form (only when built with [`router_with_pools`])
//! * `POST /orgs/{slug}/edit`    — submit edit (only when built with [`router_with_pools`])
//! * `GET  /__static__/rustango.png` — embedded asset
//!
//! Operator-side mutations are limited by design — provisioning
//! (`create-tenant`, `create-operator`) still runs through the `Cli`
//! verbs because those have side effects (CREATE SCHEMA, migrations,
//! password hashing) that don't fit a single HTTP form. The edit
//! routes cover the post-creation knobs an operator legitimately
//! needs to twiddle live: display name, host pattern, port, path
//! prefix, active flag, and the resolved `database_url` for
//! database-mode tenants. `database_url` is the only field with a
//! pool-rebuild side effect — see [`org_edit_submit`].
//!
//! ## Wiring
//!
//! ```ignore
//! let console = crate::tenancy::operator_console::router(
//!     registry.clone(),
//!     SessionSecret::from_env_or_random(),
//! );
//! let app = axum::Router::new().merge(console);
//! ```

use crate::core::Column as _;
// v0.34 — operator console no longer imports `PgPool` directly.
// ConsoleState.registry is `crate::sql::Pool` (the backend-erasing
// enum); all internal queries route through `fetch_pool` /
// `insert_pool` / `save_pool` / `update_pool`.
use crate::sql::FetcherPool;
use crate::storage::BoxedStorage;
use axum::body::Body;
use axum::extract::{Form, Multipart, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Extension, Router};
use cookie::time::Duration as CookieDuration;
use cookie::{Cookie, SameSite};
use serde::Deserialize;
use std::sync::Arc;
use tera::{Context, Tera};

// v0.38 — `session` (HMAC-signed cookies, SessionSecret) lives one
// level up at `tenancy::session` so non-PG callers like
// `DatabaseTenantContext` can reach it.
pub use super::session::{self, SessionPayload, SessionSecret, SessionSecretError};
use super::session::{COOKIE_NAME, SESSION_TTL_SECS};

use super::auth;
use super::branding::{self, BrandAssetKind};
use super::pools::TenantPools;

const RUSTANGO_PNG: &[u8] = include_bytes!("../static/rustango.png");

#[derive(Clone)]
struct ConsoleState {
    /// Backend-erasing registry pool. PG / MySQL / SQLite all share
    /// one operator-console code path — every query inside the
    /// console routes through `_pool` ORM helpers (`fetch_pool` /
    /// `insert_pool` / `save_pool`) which dispatch per-backend.
    registry: crate::sql::Pool,
    /// Optional pool cache. When `Some`, the operator console exposes
    /// the `/orgs/{slug}/edit` mutation routes — needed because a
    /// `database_url` rotation must drop the cached `TenantPool` for
    /// that org so the next request rebuilds against the new URL.
    /// When `None` (the legacy [`router`] entry point), edit routes
    /// aren't mounted and the console stays read-only.
    pools: Option<Arc<TenantPools>>,
    session_secret: Arc<SessionSecret>,
    tera: Arc<Tera>,
    /// Storage backend for per-tenant brand assets (logo, favicon).
    /// Defaults to `LocalStorage` rooted at `./var/brand` — override
    /// via `RUSTANGO_BRAND_STORAGE_DIR`.
    brand_storage: BoxedStorage,
    /// Operator console's own (non-per-tenant) brand. Read at boot
    /// from env, stamped into every render context so a deployment
    /// can rebrand the console without touching templates.
    op_brand: Arc<OpBrand>,
    /// Tenant-side session secret. When `Some`, the operator
    /// console exposes `POST /orgs/{slug}/impersonate` — a flow
    /// that mints a tenant-bound `TenantSessionPayload` with
    /// `imp = Some(operator_id)` so the operator can open the
    /// tenant admin as superuser without knowing a tenant
    /// user's password. (#78, v0.27.8)
    /// Wired by `Server::Builder::serve` (sharing the same
    /// `SessionSecret::from_env_or_disk` instance the tenant
    /// admin uses); custom mount points can opt in via the
    /// `_with_impersonation` constructor variants.
    tenant_session_secret: Option<Arc<SessionSecret>>,
    /// URL on the tenant admin where the operator console's
    /// impersonation flow lands the browser to redeem a signed
    /// handoff token (#88). Mirrors
    /// [`super::routes::RouteConfig::impersonation_handoff_url`].
    /// Replaced the v0.27.8 cookie-domain handoff (which broke
    /// on Chromium against the `localhost` PSL TLD). Default
    /// `/_impersonation_handoff`.
    tenant_handoff_url: String,
}

/// Operator console branding resolved at boot. Static for the
/// lifetime of the process; per-tenant branding lives on `Org`.
///
/// Resolution priority (most specific wins):
/// 1. `RUSTANGO_OPERATOR_*` env vars (deploy-time override)
/// 2. `[brand]` section in `config/<env>_settings.toml` (#87 wiring)
/// 3. Hardcoded defaults
#[derive(Debug, Clone)]
struct OpBrand {
    name: String,
    tagline: Option<String>,
    logo_url: String,
    primary_color: Option<String>,
    theme_mode: String,
}

impl OpBrand {
    /// Hardcoded fallback values applied first.
    fn defaults() -> Self {
        Self {
            name: "Rustango".to_owned(),
            tagline: None,
            logo_url: "/__static__/rustango.png".to_owned(),
            primary_color: None,
            theme_mode: "auto".to_owned(),
        }
    }

    /// Resolve the operator console branding from settings + env.
    /// Order: defaults → `Settings.brand` (TOML) → env vars (which
    /// win so deploy-time emergency overrides don't require a
    /// config push). Best-effort on the TOML side — a missing
    /// `config/default.toml` is silently skipped, same as
    /// `Cli::with_settings_from_env`.
    fn from_env() -> Self {
        let mut out = Self::defaults();
        #[cfg(feature = "config")]
        if let Ok(s) = crate::config::Settings::load_from_env() {
            Self::apply_brand_settings(&mut out, &s.brand);
        }
        Self::apply_env_overrides(&mut out);
        out
    }

    #[cfg(feature = "config")]
    fn apply_brand_settings(out: &mut Self, b: &crate::config::BrandSettings) {
        if let Some(n) = b.name.as_deref().filter(|s| !s.is_empty()) {
            out.name = n.to_owned();
        }
        if let Some(t) = b.tagline.as_deref().filter(|s| !s.is_empty()) {
            out.tagline = Some(t.to_owned());
        }
        if let Some(u) = b.logo_url.as_deref().filter(|s| !s.is_empty()) {
            out.logo_url = u.to_owned();
        }
        if let Some(hex) = b
            .primary_color
            .as_deref()
            .and_then(branding::validate_hex_color)
        {
            out.primary_color = Some(hex);
        }
        if let Some(mode) = b
            .theme_mode
            .as_deref()
            .and_then(branding::validate_theme_mode)
        {
            out.theme_mode = mode.to_owned();
        }
    }

    fn apply_env_overrides(out: &mut Self) {
        if let Ok(v) = std::env::var("RUSTANGO_OPERATOR_BRAND_NAME") {
            if !v.is_empty() {
                out.name = v;
            }
        }
        if let Ok(v) = std::env::var("RUSTANGO_OPERATOR_TAGLINE") {
            if !v.is_empty() {
                out.tagline = Some(v);
            }
        }
        if let Ok(v) = std::env::var("RUSTANGO_OPERATOR_LOGO_URL") {
            if !v.is_empty() {
                out.logo_url = v;
            }
        }
        if let Some(hex) = std::env::var("RUSTANGO_OPERATOR_PRIMARY_COLOR")
            .ok()
            .as_deref()
            .and_then(branding::validate_hex_color)
        {
            out.primary_color = Some(hex);
        }
        if let Some(mode) = std::env::var("RUSTANGO_OPERATOR_THEME_MODE")
            .ok()
            .as_deref()
            .and_then(branding::validate_theme_mode)
        {
            out.theme_mode = mode.to_owned();
        }
    }
}

/// Build the read-only operator-console `axum::Router`. Mount at the
/// apex (production: `app.example.com`; demo: `localhost:8080`) — the
/// console expects to live at the root, not nested under a path.
///
/// Use [`router_with_pools`] when you want operators to be able to
/// edit org config (display name, host pattern, port, path prefix,
/// active flag, `database_url`) live from the UI — that variant
/// needs the [`TenantPools`] handle to evict the cached pool when
/// `database_url` rotates.
///
/// Brand assets (logo / favicon uploads) go to the default
/// [`branding::default_brand_storage`] (a `LocalStorage` rooted at
/// `./var/brand` or `RUSTANGO_BRAND_STORAGE_DIR`). To plug in S3 /
/// R2 / B2 / MinIO / a CDN-fronted bucket, use
/// [`router_with_brand_storage`].
#[must_use]
pub fn router(registry: impl Into<crate::sql::Pool>, secret: SessionSecret) -> Router {
    router_inner(
        registry.into(),
        None,
        secret,
        branding::default_brand_storage(),
        None,
        default_tenant_handoff_url(),
    )
}

/// Like [`router`] but also exposes `GET`/`POST /orgs/{slug}/edit`,
/// powered by the supplied `TenantPools` (cache-eviction on
/// `database_url` rotation). Production tenancy `Builder` wires this
/// path so operators don't need a redeploy + manual SQL to fix a
/// stale connection URL.
#[must_use]
pub fn router_with_pools(
    registry: impl Into<crate::sql::Pool>,
    pools: Arc<TenantPools>,
    secret: SessionSecret,
) -> Router {
    router_inner(
        registry.into(),
        Some(pools),
        secret,
        branding::default_brand_storage(),
        None,
        default_tenant_handoff_url(),
    )
}

/// Like [`router_with_pools`] but also wires the
/// **operator-as-superuser tenant impersonation** flow (#78).
/// Pass the same `tenant_session_secret` your `TenantAdminBuilder`
/// uses so the handoff token the operator console mints will
/// verify in the tenant admin.
///
/// Since v0.29 (#88), the flow is a URL-token handoff instead of
/// a cookie-domain handoff: the operator console mints a signed
/// token, redirects the browser to
/// `<sub>.<apex><tenant_handoff_url>?token=<...>`, and the tenant
/// admin redeems the token + sets a host-scoped cookie. This
/// works on every browser/host combination, including Chromium
/// against `localhost` (where the older cookie-domain approach
/// broke because Chromium treats `localhost` as a public-suffix
/// TLD). The legacy `tenant_cookie_domain` parameter is gone —
/// no impersonation cookie is set on the operator-console origin.
///
/// `Server::Builder::serve` calls this automatically since v0.27.8.
/// Custom mount points opt in by replacing `router_with_pools` with
/// this variant.
#[must_use]
pub fn router_with_impersonation(
    registry: impl Into<crate::sql::Pool>,
    pools: Arc<TenantPools>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: SessionSecret,
    tenant_handoff_url: String,
) -> Router {
    router_inner(
        registry.into(),
        Some(pools),
        secret,
        brand_storage,
        Some(tenant_session_secret),
        tenant_handoff_url,
    )
}

/// Full-control entry point — takes any [`BoxedStorage`] for brand
/// asset storage. Use this with `S3Storage` (AWS / R2 / B2 / MinIO),
/// a `LocalStorage` configured with `with_base_url` for CDN
/// pre-fronting, or any user-supplied `Storage` impl. When the
/// configured backend exposes URLs via `Storage::url`, rendered
/// `<img src>` tags point straight at it — the framework's
/// `/__brand__/{slug}/{filename}` static handler is only used as
/// the fallback for backends that return `None` from `url()` (the
/// default `LocalStorage` without `with_base_url`).
///
/// `pools = Some(...)` mounts the org-edit + branding upload
/// routes; `None` keeps the console read-only (matches `router`).
#[must_use]
pub fn router_with_brand_storage(
    registry: impl Into<crate::sql::Pool>,
    pools: Option<Arc<TenantPools>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
) -> Router {
    router_inner(
        registry.into(),
        pools,
        secret,
        brand_storage,
        None,
        default_tenant_handoff_url(),
    )
}

/// Default tenant-admin URL prefix when the operator console is
/// constructed via a pre-RouteConfig entry point. Matches the
/// v0.29 friendly-by-default value from #85 so projects on
/// current rustango Just Work; legacy projects opt in via
/// `router_with_impersonation`'s `tenant_admin_url` parameter.
fn default_tenant_handoff_url() -> String {
    super::routes::RouteConfig::default().impersonation_handoff_url
}

fn router_inner(
    registry: crate::sql::Pool,
    pools: Option<Arc<TenantPools>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: Option<SessionSecret>,
    tenant_handoff_url: String,
) -> Router {
    let mut tera = Tera::default();
    tera.add_raw_templates([
        (
            "_theme_tokens.html",
            include_str!("../../styles/theme_tokens.html"),
        ),
        (
            "_op_styles.html",
            include_str!("../templates/_op_styles.html"),
        ),
        (
            "_theme_toggle.html",
            include_str!("../../admin/templates/_theme_toggle.html"),
        ),
        (
            "op_layout.html",
            include_str!("../templates/op_layout.html"),
        ),
        ("op_login.html", include_str!("../templates/op_login.html")),
        (
            "op_welcome.html",
            include_str!("../templates/op_welcome.html"),
        ),
        (
            "op_operators.html",
            include_str!("../templates/op_operators.html"),
        ),
        ("op_orgs.html", include_str!("../templates/op_orgs.html")),
        (
            "op_orgs_edit.html",
            include_str!("../templates/op_orgs_edit.html"),
        ),
        (
            "op_change_password.html",
            include_str!("../templates/op_change_password.html"),
        ),
    ])
    .expect("operator-console templates parse");
    let edit_enabled = pools.is_some();
    let impersonation_enabled = tenant_session_secret.is_some() && pools.is_some();
    let state = ConsoleState {
        registry,
        pools,
        session_secret: Arc::new(secret),
        tera: Arc::new(tera),
        brand_storage,
        op_brand: Arc::new(OpBrand::from_env()),
        tenant_session_secret: tenant_session_secret.map(Arc::new),
        tenant_handoff_url,
    };

    // Public routes (login + static asset + brand asset) skip the
    // auth gate. Brand assets are public images and need to be
    // reachable from un-authenticated tenant pages.
    let public = Router::new()
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout))
        .route("/__static__/rustango.png", get(static_rustango_png))
        .route("/__brand__/{slug}/{filename}", get(serve_brand_asset));

    // Authenticated routes: the middleware injects an `Extension<auth::Operator>`.
    let mut private = Router::new()
        .route("/", get(welcome))
        .route("/operators", get(operators_list))
        .route("/orgs", get(orgs_list))
        .route(
            "/change-password",
            get(change_password_form).post(change_password_submit),
        );
    if edit_enabled {
        private = private
            .route(
                "/orgs/{slug}/edit",
                get(org_edit_form).post(org_edit_submit),
            )
            // v0.27.10 (#68) — branding endpoint accepts POST for
            // multipart upload. Add a GET that bounces back to
            // the parent edit form so a manual URL hit (or a
            // browser session-expiry replay) doesn't 405.
            .route(
                "/orgs/{slug}/edit/branding",
                get(org_post_only_redirect).post(org_edit_branding),
            );
    }
    if impersonation_enabled {
        // v0.27.8 (#78) — operator-as-superuser tenant admin login.
        // Mints a tenant-bound impersonation cookie and 302s to
        // the tenant admin's `/__admin/`. Audit-log entry recorded
        // on every mint so each session is traceable to an
        // operator id.
        // v0.27.10 (#68) — same GET fallback as branding above.
        private = private.route(
            "/orgs/{slug}/impersonate",
            get(org_post_only_redirect).post(org_impersonate),
        );
    }
    let private = private.route_layer(middleware::from_fn_with_state(
        state.clone(),
        require_session,
    ));

    public.merge(private).with_state(state)
}

/// Stamp the operator-console branding fields onto every render
/// context. Centralizing the keys keeps op_layout.html and op_login.html
/// in sync without each handler remembering the four template names.
fn inject_op_brand(ctx: &mut Context, brand: &OpBrand) {
    ctx.insert("brand_name", &brand.name);
    ctx.insert("brand_tagline", &brand.tagline);
    ctx.insert("brand_logo_url", &brand.logo_url);
    ctx.insert("theme_mode", &brand.theme_mode);
    ctx.insert(
        "brand_css",
        &branding::build_op_brand_css(brand.primary_color.as_deref()),
    );
}

// ----------------------------- session middleware

async fn require_session(
    State(state): State<ConsoleState>,
    headers: HeaderMap,
    uri: Uri,
    mut req: axum::http::Request<Body>,
    next: Next,
) -> Response<Body> {
    let cookie_value = read_cookie(&headers, COOKIE_NAME);
    let payload = cookie_value
        .as_deref()
        .and_then(|v| session::decode(&state.session_secret, v).ok());
    // v0.27.10 (#68) — when a non-GET request hits an expired
    // session, redirecting straight to login makes the browser
    // turn the original POST into a GET on the way back (303 →
    // GET), which then 405s on POST-only routes like
    // `/orgs/{slug}/edit/branding` and `/orgs/{slug}/impersonate`.
    // Sanitize the `next` URL down to the parent GET URL before
    // the bounce. The operator loses unsaved form data either
    // way; at least they don't see a 405 page.
    let method = req.method().clone();
    let raw_next = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let safe_next = sanitize_next_for_method(&method, raw_next);
    let Some(payload) = payload else {
        return redirect_to_login(&safe_next).into_response();
    };
    match auth::Operator::objects()
        .where_(auth::Operator::id.eq(payload.oid))
        .fetch_pool(&state.registry)
        .await
    {
        Ok(rows) => {
            let Some(op) = rows.into_iter().next().filter(|o| o.active) else {
                return redirect_to_login(&safe_next).into_response();
            };
            // v0.28.4 (#77) — invalidate sessions issued before the
            // operator's last password rotation. NULL means the
            // operator hasn't rotated since v0.28.4 — those
            // sessions stay valid.
            if let Some(ts) = op.password_changed_at {
                if payload.iat < ts.timestamp() {
                    return redirect_to_login(&safe_next).into_response();
                }
            }
            req.extensions_mut().insert(op);
            next.run(req).await
        }
        Err(e) => {
            tracing::warn!(target: "crate::tenancy::operator_console", error = %e, "registry lookup");
            (StatusCode::INTERNAL_SERVER_ERROR, "registry lookup failed").into_response()
        }
    }
}

/// v0.27.10 (#68) — GET handler for POST-only sub-form routes
/// (`/orgs/{slug}/edit/branding`, `/orgs/{slug}/impersonate`).
/// A bare GET on these used to 405; now it bounces back to
/// the parent edit form so the operator lands somewhere
/// useful instead of staring at a Method-Not-Allowed page.
async fn org_post_only_redirect(
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Redirect {
    Redirect::to(&format!("/orgs/{slug}/edit"))
}

/// v0.27.10 (#68) — when an unauthenticated non-GET request
/// would round-trip through `/login?next=…` and back, the
/// resulting GET re-issues the original URL — which 405s on
/// POST-only routes. Rewrite `path` to a safe-GET equivalent
/// based on the original method.
///
/// Conservative table: when the method is GET / HEAD, the
/// raw path is fine. Otherwise we strip back to the closest
/// known parent that has a GET handler. For paths we don't
/// recognize, we strip down to `/` so the operator at least
/// lands somewhere reachable.
fn sanitize_next_for_method(method: &axum::http::Method, path: &str) -> String {
    if method == axum::http::Method::GET || method == axum::http::Method::HEAD {
        return path.to_owned();
    }
    // Drop query string so the rewrite is path-only.
    let path_only = path.split('?').next().unwrap_or(path);
    // Known operator-console POST-only routes → parent GET URL.
    // Pattern matching is intentionally explicit (a regex would
    // hide the route taxonomy here).
    if let Some(rest) = path_only.strip_prefix("/orgs/") {
        // /orgs/{slug}/edit/branding → /orgs/{slug}/edit
        // /orgs/{slug}/impersonate    → /orgs/{slug}/edit
        // /orgs/{slug}/edit          → /orgs/{slug}/edit (GET form)
        if let Some(slug_end) = rest.find('/') {
            let slug = &rest[..slug_end];
            return format!("/orgs/{slug}/edit");
        }
    }
    // Unknown POST path — fall back to the welcome page so the
    // operator isn't stranded.
    "/".to_owned()
}

fn redirect_to_login(next_path: &str) -> Response<Body> {
    let next = if next_path == "/login" || next_path.starts_with("/logout") {
        "/".to_string()
    } else {
        next_path.to_string()
    };
    let location = format!("/login?next={}", urlencoding_lite(&next));
    Redirect::to(&location).into_response()
}

// ----------------------------- /login

#[derive(Deserialize)]
struct LoginQuery {
    #[serde(default)]
    next: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

async fn login_form(
    State(state): State<ConsoleState>,
    Query(q): Query<LoginQuery>,
) -> Html<String> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("next", &q.next.unwrap_or_else(|| "/".into()));
    ctx.insert("error", &q.error);
    Html(state.tera.render("op_login.html", &ctx).unwrap_or_default())
}

#[derive(Deserialize)]
struct LoginSubmit {
    username: String,
    password: String,
    #[serde(default)]
    next: Option<String>,
}

async fn login_submit(
    State(state): State<ConsoleState>,
    Form(form): Form<LoginSubmit>,
) -> Response<Body> {
    let next = sanitize_next(form.next.as_deref());
    let principal =
        match auth::authenticate_operator_pool(&state.registry, &form.username, &form.password)
            .await
        {
            Ok(Some(op)) => op,
            Ok(None) => {
                return Redirect::to(&format!(
                    "/login?error=Invalid+credentials&next={}",
                    urlencoding_lite(&next)
                ))
                .into_response();
            }
            Err(e) => {
                tracing::warn!(target: "crate::tenancy::operator_console", error = %e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
            }
        };
    let oid = principal.id.get().copied().unwrap_or_default();
    let payload = SessionPayload::new(oid, SESSION_TTL_SECS);
    let cookie_value = session::encode(&state.session_secret, &payload);
    let cookie = Cookie::build((COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(SESSION_TTL_SECS))
        .build();
    let mut resp = Redirect::to(&next).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("cookie is ascii"),
    );
    resp
}

async fn logout(State(_state): State<ConsoleState>) -> Response<Body> {
    let clear = Cookie::build((COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(0))
        .build();
    let mut resp = Redirect::to("/login").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear.to_string()).expect("cookie is ascii"),
    );
    resp
}

// ----------------------------- views

/// `GET /change-password` — render the operator self-serve
/// change-password form (#77, v0.29). Lives behind
/// `require_session` so unauthenticated requests bounce to login.
async fn change_password_form(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Html<String> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "change_password");
    ctx.insert("operator_username", &op.username);
    ctx.insert("error", &params.get("error"));
    ctx.insert("success", &params.get("ok"));
    Html(
        state
            .tera
            .render("op_change_password.html", &ctx)
            .unwrap_or_else(|e| {
                tracing::error!(target: "crate::tenancy::operator_console", error = %e, "op_change_password.html render");
                "<!doctype html><h1>Change-password page unavailable</h1>".to_owned()
            }),
    )
}

#[derive(Debug, serde::Deserialize)]
struct OpChangePasswordForm {
    current_password: String,
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

/// `POST /change-password` — verify the operator's current
/// password, hash the new one, persist it + bump
/// `password_changed_at`. The session middleware
/// (`require_session`) already invalidates cookies whose
/// `iat` predates `password_changed_at`, so the session this
/// request is running on may be the LAST request that cookie
/// can serve — the next click bounces to login. Mirror of
/// `tenancy::admin::change_password_submit` for tenant users.
async fn change_password_submit(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<OpChangePasswordForm>,
) -> Response<Body> {
    let redir = |query: &str| -> Response<Body> {
        Redirect::to(&format!("/change-password?{query}")).into_response()
    };
    let redir_err = |msg: &str| redir(&format!("error={}", crate::url_codec::url_encode(msg)));

    if form.current_password.is_empty() || form.new_password.is_empty() {
        return redir_err("All fields are required.");
    }
    if !form.confirm_password.is_empty() && form.confirm_password != form.new_password {
        return redir_err("New password and confirmation did not match.");
    }
    if form.new_password == form.current_password {
        return redir_err("New password must differ from the current password.");
    }
    if form.new_password.chars().count() < 8 {
        return redir_err("New password must be at least 8 characters.");
    }

    let op_id = op.id.get().copied().unwrap_or(0);
    if op_id <= 0 {
        return redir_err("Session is missing an operator id; please log in again.");
    }

    // Re-fetch the canonical Operator row via the ORM so the lookup
    // and the subsequent password rotate are bi-dialect. `op` from
    // the extension is a snapshot taken in `require_session`; using
    // the live registry row matches the tenant flow + protects
    // against the rare race where a peer operator just rotated this
    // account.
    let mut op_row: auth::Operator = match auth::Operator::objects()
        .where_(auth::Operator::id.eq(op_id))
        .fetch_pool(&state.registry)
        .await
    {
        Ok(rows) => match rows.into_iter().next() {
            Some(r) => r,
            None => {
                return redir_err("Your account no longer exists; please log in again.");
            }
        },
        Err(e) => {
            tracing::warn!(target: "crate::tenancy::operator_console", error = %e, "change-password lookup");
            return (StatusCode::INTERNAL_SERVER_ERROR, "lookup failed").into_response();
        }
    };
    let ok =
        super::password::verify(&form.current_password, &op_row.password_hash).unwrap_or(false);
    if !ok {
        return redir_err("Current password did not match.");
    }
    let new_hash = match super::password::hash(&form.new_password) {
        Ok(h) => h,
        Err(e) => return redir_err(&format!("hash failed: {e}")),
    };
    op_row.password_hash = new_hash;
    op_row.password_changed_at = Some(chrono::Utc::now());
    if let Err(e) = op_row.save_pool(&state.registry).await {
        tracing::warn!(target: "crate::tenancy::operator_console", error = %e, "change-password update");
        return (StatusCode::INTERNAL_SERVER_ERROR, "update failed").into_response();
    }
    redir("ok=Password+updated")
}

async fn welcome(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Html<String> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "home");
    ctx.insert("operator_username", &op.username);
    Html(
        state
            .tera
            .render("op_welcome.html", &ctx)
            .unwrap_or_default(),
    )
}

async fn operators_list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    let rows: Vec<auth::Operator> =
        match auth::Operator::objects().fetch_pool(&state.registry).await {
            Ok(r) => r,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
        };
    let view: Vec<_> = rows
        .into_iter()
        .map(|o| {
            serde_json::json!({
                "id": o.id.get().copied().unwrap_or_default(),
                "username": o.username,
                "active": o.active,
                "created_at": o.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            })
        })
        .collect();
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "operators");
    ctx.insert("operator_username", &op.username);
    ctx.insert("operators", &view);
    Html(
        state
            .tera
            .render("op_operators.html", &ctx)
            .unwrap_or_default(),
    )
    .into_response()
}

async fn orgs_list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    let rows: Vec<super::Org> = match super::Org::objects().fetch_pool(&state.registry).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let view: Vec<_> = rows
        .into_iter()
        .map(|o| {
            serde_json::json!({
                "slug": o.slug,
                "display_name": o.display_name,
                "storage_mode": o.storage_mode,
                "backend_kind": o.backend_kind,
                "host_pattern": o.host_pattern,
                "active": o.active,
                "created_at": o.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            })
        })
        .collect();
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("orgs", &view);
    ctx.insert("edit_enabled", &state.pools.is_some());
    Html(state.tera.render("op_orgs.html", &ctx).unwrap_or_default()).into_response()
}

// ----------------------------- /orgs/{slug}/edit
//
// Reuses the framework's existing admin form pipeline:
// * [`crate::admin::render::render_input`] turns each `FieldSchema`
//   into the right `<input>` HTML — number for ints, checkbox for
//   bool, text/textarea for strings, datetime-local for timestamps.
// * [`crate::admin::render::render_value_for_input`] reads the
//   current value out of the row as a prefill string.
// * [`crate::forms::collect_values`] parses the submitted form
//   against `Org::SCHEMA` and produces `(column, SqlValue)` pairs
//   with full per-field bound checks (max_length, min/max, type).
//
// What stays bespoke:
// * Lock list — `slug`, `storage_mode`, `schema_name`, `id`,
//   `created_at` must not be editable from this surface (would
//   orphan tenant data or break invariants).
// * `database_url` masking — never echo the existing literal back to
//   the browser; show only the secret-reference shape (e.g.
//   `env:DATABASE_URL_ACME`) and treat empty submit as "keep current".
// * Pool eviction on `database_url` change — calls
//   [`TenantPools::invalidate`] so the next request rebuilds the
//   cached pool with new credentials.

/// Names of `Org` fields that are display-only on the edit form.
/// `logo_path` / `favicon_path` are populated by the multipart
/// upload sub-form (`POST /orgs/{slug}/edit/branding`) — never via
/// the regular config edit, so they live in the locked section.
///
/// `backend_kind` (v0.33) is locked too — changing the backend mid-life
/// would orphan the tenant's data on the old driver. The
/// `migrate-tenant-storage` verb is the supported migration path
/// (issue #58 once it gains a backend-translation step).
const LOCKED_ORG_FIELDS: &[&str] = &[
    "id",
    "slug",
    "storage_mode",
    "backend_kind",
    "schema_name",
    "created_at",
    "logo_path",
    "favicon_path",
];

/// `database_url` is editable but special: empty submit means "keep
/// current", and we never render the existing value into the input.
const DATABASE_URL_FIELD: &str = "database_url";

#[derive(Deserialize, Default)]
struct OrgEditQuery {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    notice: Option<String>,
}

async fn org_edit_form(
    State(state): State<ConsoleState>,
    axum::extract::Path(slug): axum::extract::Path<String>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<OrgEditQuery>,
) -> Response<Body> {
    use crate::admin::render;
    use crate::core::Model as _;

    // Fetch via the ORM (bi-dialect) instead of `SELECT *` + PgRow.
    let rows: Vec<super::Org> = match super::Org::objects()
        .where_(super::Org::slug.eq(slug.clone()))
        .fetch_pool(&state.registry)
        .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let Some(org_row) = rows.into_iter().next() else {
        return (StatusCode::NOT_FOUND, format!("org `{slug}` not found")).into_response();
    };
    // Serialize once to a JSON object so the per-field renderer can
    // read each column by name without us hand-rolling a per-field
    // match. Backend-agnostic — no PgRow.
    let row_json = serde_json::to_value(&org_row).unwrap_or_else(|_| serde_json::json!({}));

    // Build per-field render contexts. Iterating `Org::SCHEMA.fields`
    // means new columns added to Org show up automatically — the
    // template doesn't need a manual update.
    let mut editable_rows: Vec<serde_json::Value> = Vec::new();
    let mut locked_rows: Vec<serde_json::Value> = Vec::new();
    for field in super::Org::SCHEMA.scalar_fields() {
        let prefill = render::render_value_for_input_json(&row_json, field);
        if LOCKED_ORG_FIELDS.contains(&field.name) {
            locked_rows.push(serde_json::json!({
                "name": field.name,
                "value": prefill,
            }));
            continue;
        }
        // database_url: mask the prefill, supply a placeholder, and
        // surface the secret-reference shape separately.
        let (prefill_for_input, helper) = if field.name == DATABASE_URL_FIELD {
            let hint = if prefill.starts_with("env:") || prefill.starts_with("vault:") {
                Some(format!("current: {prefill}"))
            } else if !prefill.is_empty() {
                Some("current: <literal connection URL — masked>".to_owned())
            } else {
                None
            };
            (
                String::new(),
                Some(format!(
                    "{} — leave blank to keep current; new value evicts the cached pool",
                    hint.unwrap_or_else(|| "no value set".to_owned())
                )),
            )
        } else {
            (prefill, None::<String>)
        };
        let input_html = render::render_input(field, &prefill_for_input, false);
        editable_rows.push(serde_json::json!({
            "name": field.name,
            "input": input_html,
            "helper": helper,
        }));
    }

    // Pull current logo / favicon paths off the org row.
    let logo_path: Option<String> = org_row.logo_path.clone();
    let favicon_path: Option<String> = org_row.favicon_path.clone();
    let logo_url = branding::brand_asset_url(&slug, logo_path.as_deref(), &state.brand_storage);
    let favicon_url =
        branding::brand_asset_url(&slug, favicon_path.as_deref(), &state.brand_storage);

    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("slug", &slug);
    ctx.insert("editable_rows", &editable_rows);
    ctx.insert("locked_rows", &locked_rows);
    ctx.insert("logo_url", &logo_url);
    ctx.insert("favicon_url", &favicon_url);
    ctx.insert("error", &q.error);
    ctx.insert("notice", &q.notice);
    // v0.27.8 (#78) — show the "Open admin as superuser →"
    // form only when the operator console was wired with a
    // tenant session secret (i.e. via `router_with_impersonation`).
    ctx.insert(
        "impersonate_enabled",
        &state.tenant_session_secret.is_some(),
    );
    Html(
        state
            .tera
            .render("op_orgs_edit.html", &ctx)
            .unwrap_or_default(),
    )
    .into_response()
}

/// `POST /orgs/{slug}/edit` — apply the changes via
/// [`crate::forms::collect_values`] (same parser the per-app admin
/// uses on `update_submit`) and emit a partial UPDATE for only the
/// columns the form actually supplied.
///
/// Side effects:
/// * `database_url` change → [`TenantPools::invalidate(slug)`] so
///   the next request to that tenant rebuilds the pool with new
///   credentials.
/// * `active = false` → resolver chain returns 404 for that tenant.
async fn org_edit_submit(
    State(state): State<ConsoleState>,
    axum::extract::Path(slug): axum::extract::Path<String>,
    Extension(op): Extension<auth::Operator>,
    Form(mut form): Form<std::collections::HashMap<String, String>>,
) -> Response<Body> {
    use crate::core::Model as _;

    let pools = state
        .pools
        .as_ref()
        .expect("edit routes only mounted when pools is Some");

    // `database_url` blank → operator wants to keep the current
    // value. Strip from the form AND extend the skip list so
    // `collect_values` doesn't even consider it (otherwise the
    // missing-field would be parsed as NULL and the row's
    // existing url would be wiped on submit).
    let database_url_supplied = form
        .get(DATABASE_URL_FIELD)
        .is_some_and(|s| !s.trim().is_empty());
    if !database_url_supplied {
        form.remove(DATABASE_URL_FIELD);
    }
    let mut skip: Vec<&str> = LOCKED_ORG_FIELDS.to_vec();
    if !database_url_supplied {
        skip.push(DATABASE_URL_FIELD);
    }
    // Bool-checkbox: HTML omits the field when unchecked. The admin's
    // collect_values pipeline understands that via `parse_form_value`,
    // which returns `false` for missing bool fields. Nothing to do
    // here — just trust the parser.

    let collected = match crate::forms::collect_values(super::Org::SCHEMA, &form, &skip) {
        Ok(v) => v,
        Err(e) => return redirect_with_error(&slug, &e.to_string()),
    };
    if collected.is_empty() {
        return redirect_with_error(&slug, "no editable fields supplied");
    }

    // Fetch existing for change detection (database_url rotation).
    // ORM path so registry-backend stays plug-and-play.
    let existing_orgs: Vec<super::Org> = match super::Org::objects()
        .where_(super::Org::slug.eq(slug.clone()))
        .fetch_pool(&state.registry)
        .await
    {
        Ok(rows) => rows,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let Some(existing_org) = existing_orgs.into_iter().next() else {
        return (StatusCode::NOT_FOUND, format!("org `{slug}` not found")).into_response();
    };
    let new_database_url = collected.iter().find_map(|(c, v)| {
        if *c == DATABASE_URL_FIELD {
            match v {
                crate::core::SqlValue::String(s) => Some(s.clone()),
                _ => None,
            }
        } else {
            None
        }
    });
    let database_url_changed = new_database_url
        .as_deref()
        .is_some_and(|new| existing_org.database_url.as_deref() != Some(new));

    // Build the UPDATE through the ORM's `UpdateQuery` IR + run it
    // via `update_pool` so the SQL gets compiled with the right
    // dialect (PG `$N` / MySQL `?` / SQLite `?`) + identifier
    // quoting. Replaces the prior hand-rolled `UPDATE "…" SET … = $N
    // WHERE …` string which was PG-only.
    let assignments: Vec<crate::core::Assignment> = collected
        .iter()
        .map(|(col, val)| crate::core::Assignment {
            column: *col,
            value: val.clone(),
        })
        .collect();
    let update_q = crate::core::UpdateQuery {
        model: super::Org::SCHEMA,
        set: assignments,
        where_clause: crate::core::WhereExpr::and_predicates(vec![crate::core::Filter {
            column: "slug",
            op: crate::core::Op::Eq,
            value: crate::core::SqlValue::String(slug.clone()),
        }]),
    };
    if let Err(e) = crate::sql::update_pool(&state.registry, &update_q).await {
        return redirect_with_error(&slug, &format!("update failed: {e}"));
    }

    if database_url_changed {
        pools.invalidate(&slug).await;
    }

    // Audit row: operator-side config edits should leave a trail
    // alongside impersonation. We record the columns touched,
    // omitting `database_url` itself even on rotation (it's a
    // credentialed URL — we record the FACT of rotation, not the
    // value).
    let operator_id = op.id.get().copied().unwrap_or(0);
    let mut detail = serde_json::Map::new();
    detail.insert(
        "action".into(),
        serde_json::Value::String("org.edit".into()),
    );
    let touched_cols: Vec<String> = collected
        .iter()
        .filter_map(|(c, _)| {
            if *c == DATABASE_URL_FIELD {
                None
            } else {
                Some((*c).to_owned())
            }
        })
        .collect();
    detail.insert("fields".into(), serde_json::json!(touched_cols));
    if database_url_changed {
        detail.insert("database_url_rotated".into(), serde_json::json!(true));
    }
    emit_op_audit(&state.registry, &slug, operator_id, "edit", detail).await;

    let notice = if database_url_changed {
        format!("updated `{slug}` (pool evicted — next request rebuilds with new URL)")
    } else {
        format!("updated `{slug}`")
    };
    Redirect::to(&format!(
        "/orgs/{}/edit?notice={}",
        urlencoding_lite(&slug),
        urlencoding_lite(&notice),
    ))
    .into_response()
}

fn redirect_with_error(slug: &str, msg: &str) -> Response<Body> {
    Redirect::to(&format!(
        "/orgs/{}/edit?error={}",
        urlencoding_lite(slug),
        urlencoding_lite(msg),
    ))
    .into_response()
}

/// Emit one audit row for an operator-side action against
/// `rustango_orgs`. Always opens with `tenant_slug` + `operator_id`
/// in the changes blob; callers add per-action keys via `extra`.
/// Failure is logged but never blocks the primary workflow — same
/// contract as the impersonation audit emission.
///
/// `source` shape: `operator:<id>:<verb>` (e.g.
/// `operator:1:impersonating`, `operator:1:edit`,
/// `operator:1:branding`). Lets post-hoc forensics filter
/// operator activity from tenant-user activity.
async fn emit_op_audit(
    registry: &crate::sql::Pool,
    slug: &str,
    operator_id: i64,
    verb: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) {
    let mut changes = serde_json::Map::new();
    changes.insert(
        "tenant_slug".into(),
        serde_json::Value::String(slug.to_owned()),
    );
    changes.insert("operator_id".into(), serde_json::json!(operator_id));
    for (k, v) in extra {
        changes.insert(k, v);
    }
    let entry = crate::audit::PendingEntry {
        entity_table: "rustango_orgs",
        entity_pk: slug.to_owned(),
        operation: crate::audit::AuditOp::Action,
        source: crate::audit::AuditSource::Custom(format!("operator:{operator_id}:{verb}")),
        changes: serde_json::Value::Object(changes),
    };
    if let Err(e) = crate::audit::emit_one_pool(registry, &entry).await {
        tracing::warn!(
            target: "crate::tenancy::operator_console",
            error = %e,
            slug = slug,
            operator_id,
            verb,
            "failed to record operator action in audit log",
        );
    }
}

// v0.34 — `bind_sql_value` was used by the old hand-rolled UPDATE
// path. The dynamic UPDATE now goes through
// `crate::sql::update_pool(&Pool, &UpdateQuery)` which compiles
// per-dialect SQL + binds via the ORM's internal `bind_query`. The
// hand-rolled helper is dead code; left removed.

// ----------------------------- /orgs/{slug}/edit/branding (multipart)
//
// The main `/orgs/{slug}/edit` form is `application/x-www-form-urlencoded`
// and posts the org's scalar config. Asset uploads need multipart, so
// they ride a dedicated sub-form. Each part is independently validated
// (content-type + size) by `branding::save_brand_asset`. After the
// file lands on disk we update the matching `Org.{logo,favicon}_path`
// column so subsequent renders pick it up.
async fn org_edit_branding(
    State(state): State<ConsoleState>,
    axum::extract::Path(slug): axum::extract::Path<String>,
    Extension(op): Extension<auth::Operator>,
    mut mp: Multipart,
) -> Response<Body> {
    let mut updates: Vec<(&'static str, Option<String>)> = Vec::new();
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().map(str::to_owned);
        let kind = match name.as_deref() {
            Some("logo") => BrandAssetKind::Logo,
            Some("favicon") => BrandAssetKind::Favicon,
            _ => continue,
        };
        let content_type = field.content_type().map(str::to_owned);
        // An empty file part means "user didn't choose a file" — skip
        // without touching the column. Browsers send the part with
        // a filename of "" and zero bytes when the input is empty.
        let bytes = match field.bytes().await {
            Ok(b) if b.is_empty() => continue,
            Ok(b) => b.to_vec(),
            Err(e) => return redirect_with_error(&slug, &format!("multipart: {e}")),
        };
        match branding::save_brand_asset(
            &slug,
            kind,
            &bytes,
            content_type.as_deref(),
            &state.brand_storage,
        )
        .await
        {
            Ok(filename) => {
                let column = match kind {
                    BrandAssetKind::Logo => "logo_path",
                    BrandAssetKind::Favicon => "favicon_path",
                };
                updates.push((column, Some(filename)));
            }
            Err(branding::BrandError::TooLarge { actual, max }) => {
                return redirect_with_error(
                    &slug,
                    &format!("file too large: {actual} bytes (max {max})"),
                );
            }
            Err(branding::BrandError::UnsupportedContentType(ct)) => {
                return redirect_with_error(
                    &slug,
                    &format!("unsupported file type `{ct}` — use PNG/JPEG/WebP/ICO"),
                );
            }
            Err(e) => return redirect_with_error(&slug, &format!("upload failed: {e}")),
        }
    }
    if updates.is_empty() {
        return redirect_with_error(&slug, "no file chosen");
    }
    // Apply all updates via the ORM's `UpdateQuery` so the SQL is
    // compiled with the right dialect (PG `$N` vs MySQL/SQLite `?`)
    // and identifier quoting per backend.
    use crate::core::Model as _;
    let assignments: Vec<crate::core::Assignment> = updates
        .iter()
        .map(|(col, v)| crate::core::Assignment {
            column: *col,
            value: v
                .as_ref()
                .map(|s| crate::core::SqlValue::String(s.clone()))
                .unwrap_or(crate::core::SqlValue::Null),
        })
        .collect();
    let update_q = crate::core::UpdateQuery {
        model: super::Org::SCHEMA,
        set: assignments,
        where_clause: crate::core::WhereExpr::and_predicates(vec![crate::core::Filter {
            column: "slug",
            op: crate::core::Op::Eq,
            value: crate::core::SqlValue::String(slug.clone()),
        }]),
    };
    if let Err(e) = crate::sql::update_pool(&state.registry, &update_q).await {
        return redirect_with_error(&slug, &format!("update failed: {e}"));
    }

    // Audit row — branding uploads touch the public-facing surface
    // of a tenant; operators iterating during onboarding leave a
    // breadcrumb trail. Records which assets landed (`logo`,
    // `favicon`) without the binary blob.
    let operator_id = op.id.get().copied().unwrap_or(0);
    let assets: Vec<String> = updates
        .iter()
        .map(|(col, _)| match *col {
            "logo_path" => "logo".to_owned(),
            "favicon_path" => "favicon".to_owned(),
            other => other.to_owned(),
        })
        .collect();
    let mut detail = serde_json::Map::new();
    detail.insert(
        "action".into(),
        serde_json::Value::String("org.branding.upload".into()),
    );
    detail.insert("assets".into(), serde_json::json!(assets));
    emit_op_audit(&state.registry, &slug, operator_id, "branding", detail).await;

    let notice = format!("uploaded {} brand asset(s) for `{slug}`", updates.len());
    Redirect::to(&format!(
        "/orgs/{}/edit?notice={}",
        urlencoding_lite(&slug),
        urlencoding_lite(&notice),
    ))
    .into_response()
}

/// `GET /__brand__/{slug}/{filename}` — public asset serve. Validates
/// the slug + filename via the branding module, reads bytes from the
/// brand storage, returns with the correct `Content-Type` and a
/// short cache TTL (operators may iterate during onboarding).
async fn serve_brand_asset(
    State(state): State<ConsoleState>,
    axum::extract::Path((slug, filename)): axum::extract::Path<(String, String)>,
) -> Response<Body> {
    match branding::load_brand_asset(&slug, &filename, &state.brand_storage).await {
        Ok((bytes, ct)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, ct)
            .header(header::CACHE_CONTROL, "public, max-age=300")
            .body(Body::from(bytes))
            .expect("response builds"),
        Err(
            branding::BrandError::NotFound
            | branding::BrandError::InvalidSlug
            | branding::BrandError::InvalidFilename,
        ) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            tracing::warn!(target: "crate::tenancy::operator_console", error = %e, "brand asset");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

// ----------------------------- static asset

async fn static_rustango_png() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from(RUSTANGO_PNG))
        .expect("response builds")
}

// ----------------------------- helpers

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

/// Minimal URL-encoder for the small set of characters we need to
/// quote in a `next=` query param. Avoids pulling in `urlencoding`
/// as a dep for ~6 lines of work.
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

/// Drop suspicious next paths — only allow same-origin relative
/// targets so an attacker can't redirect post-login to an external
/// site.
fn sanitize_next(next: Option<&str>) -> String {
    match next {
        Some(s) if s.starts_with('/') && !s.starts_with("//") && !s.contains("://") => s.to_owned(),
        _ => "/".to_owned(),
    }
}

// ============================================================== /orgs/{slug}/impersonate
//
// v0.27.8 (#78) — "Open admin as superuser →" button on
// `/orgs/{slug}/edit`. Originally minted a tenant-bound
// `TenantSessionPayload` cookie on the apex domain and 302'd to
// the tenant admin. v0.29 (#88) flipped the flow to a URL-token
// handoff: the operator console mints a short-lived signed
// `HandoffPayload` and 302s to
// `<sub>.<apex><handoff_url>?token=<...>`. The tenant admin
// redeems the token + sets a host-scoped cookie, which Chromium
// accepts even on the `localhost` PSL TLD where the cookie-domain
// approach failed.
//
// Banner + audit-log entries on both ends still make impersonation
// visible + traceable.

async fn org_impersonate(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    headers: HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response<Body> {
    let Some(tenant_secret) = state.tenant_session_secret.clone() else {
        // Should never happen — the route is only mounted when
        // the secret was supplied. Defensive guard.
        return (StatusCode::SERVICE_UNAVAILABLE, "impersonation disabled").into_response();
    };
    // Look up the org so we can refuse impersonation against
    // inactive tenants, and so the audit-log entry has the
    // correct context.
    let orgs: Vec<super::Org> = match super::Org::objects()
        .where_(super::Org::slug.eq(slug.clone()))
        .fetch_pool(&state.registry)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(target: "crate::tenancy::operator_console", error = %e, "org lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "registry lookup failed").into_response();
        }
    };
    let Some(org) = orgs.into_iter().next() else {
        return (StatusCode::NOT_FOUND, format!("no tenant `{slug}`")).into_response();
    };
    if !org.active {
        return (
            StatusCode::CONFLICT,
            format!("tenant `{slug}` is inactive — refusing impersonation"),
        )
            .into_response();
    }
    let operator_id = op.id.get().copied().unwrap_or(0);

    // Mint the short-lived URL handoff token. Includes a random
    // single-use `jti` and the slug, both checked at redemption.
    use super::impersonation_handoff as handoff;
    let payload =
        handoff::HandoffPayload::new(operator_id, slug.clone(), handoff::HANDOFF_TTL_SECS);
    let token = handoff::mint(&tenant_secret, &payload);

    // Audit-log entry on the operator side. The tenant admin
    // emits a separate entry the first time an impersonation
    // session lands on a write — both ends are visible.
    let mut detail = serde_json::Map::new();
    detail.insert(
        "action".into(),
        serde_json::Value::String("impersonate.start".into()),
    );
    emit_op_audit(&state.registry, &slug, operator_id, "impersonating", detail).await;

    // Build the redirect URL: tenant subdomain + the configured
    // impersonation handoff path + `?token=...`. The tenant admin
    // redeems the token, sets the impersonation cookie host-scoped
    // to its own subdomain, and bounces the browser onward to the
    // admin index.
    //
    // Scheme: respect `RUSTANGO_TENANT_SCHEME` for explicit
    // overrides; otherwise default to http for local dev.
    let scheme = std::env::var("RUSTANGO_TENANT_SCHEME").unwrap_or_else(|_| "http".into());
    let host = if let Some(pat) = org.host_pattern.as_deref().filter(|s| !s.is_empty()) {
        pat.to_owned()
    } else {
        // Fall back to apex composition: <slug>.<apex>. The
        // apex isn't directly in ConsoleState; pull from env
        // as the operator console already does in OpBrand.
        let apex = std::env::var("RUSTANGO_APEX_DOMAIN").unwrap_or_else(|_| "localhost".into());
        format!("{}.{}", slug, apex)
    };
    // Port: prefer the explicit `RUSTANGO_TENANT_PORT` env var
    // (deployments where the listener and the public-facing port
    // differ — e.g. behind a reverse proxy). Otherwise reuse the
    // port from the inbound request's Host header so dev (`:8080`)
    // and apex-on-standard-port prod (no port suffix) both Just
    // Work without configuration.
    let port_suffix = std::env::var("RUSTANGO_TENANT_PORT")
        .ok()
        .filter(|s| !s.is_empty() && s != "80" && s != "443")
        .map(|p| format!(":{p}"))
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .and_then(|h| h.rsplit_once(':').map(|(_, port)| port.to_owned()))
                .filter(|p| !p.is_empty() && p != "80" && p != "443")
                .map(|p| format!(":{p}"))
        })
        .unwrap_or_default();
    let handoff_path = state.tenant_handoff_url.trim_end_matches('/');
    // The token is base64url (`URL_SAFE_NO_PAD`) + a single `.` —
    // every character is already URL-safe, so no escaping needed.
    let redirect_to = format!("{scheme}://{host}{port_suffix}{handoff_path}?token={token}");

    let mut resp = Redirect::to(&redirect_to).into_response();
    // The token in the URL is single-use + short-lived, but
    // `Referrer-Policy: no-referrer` keeps it from leaking to
    // any third-party resource the destination page loads.
    resp.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    tracing::info!(
        target: "crate::tenancy::operator_console",
        slug = %slug,
        operator_id,
        ttl_secs = handoff::HANDOFF_TTL_SECS,
        redirect_to = %redirect_to,
        "minted impersonation handoff token",
    );
    resp
}

#[cfg(test)]
mod sanitize_next_method_tests {
    use super::sanitize_next_for_method;
    use axum::http::Method;

    // v0.27.10 (#68) — guard against the regression that made
    // a POST → /login?next=… → POST chain land on a 405.

    #[test]
    fn get_passes_through_unchanged() {
        assert_eq!(
            sanitize_next_for_method(&Method::GET, "/orgs/acme/edit"),
            "/orgs/acme/edit"
        );
        assert_eq!(
            sanitize_next_for_method(&Method::HEAD, "/anywhere"),
            "/anywhere"
        );
    }

    #[test]
    fn post_to_branding_rewrites_to_parent_edit() {
        assert_eq!(
            sanitize_next_for_method(&Method::POST, "/orgs/acme/edit/branding"),
            "/orgs/acme/edit"
        );
    }

    #[test]
    fn post_to_impersonate_rewrites_to_parent_edit() {
        assert_eq!(
            sanitize_next_for_method(&Method::POST, "/orgs/acme/impersonate"),
            "/orgs/acme/edit"
        );
    }

    #[test]
    fn post_to_edit_rewrites_to_get_edit_form() {
        assert_eq!(
            sanitize_next_for_method(&Method::POST, "/orgs/acme/edit"),
            "/orgs/acme/edit"
        );
    }

    #[test]
    fn unknown_post_path_falls_back_to_root() {
        assert_eq!(
            sanitize_next_for_method(&Method::POST, "/some/random/post"),
            "/"
        );
    }

    #[test]
    fn query_string_dropped_from_rewrite_target() {
        // Operator submitted a form with extra query — we don't
        // try to preserve it across the rewrite. The point is
        // that they land on a GET-able page, not that we
        // re-execute the form.
        assert_eq!(
            sanitize_next_for_method(&Method::POST, "/orgs/acme/impersonate?return=foo"),
            "/orgs/acme/edit"
        );
    }
}

#[cfg(test)]
mod opbrand_tests {
    use super::OpBrand;

    /// Hardcoded fallback values when nothing is configured.
    #[test]
    fn defaults_match_documented_values() {
        let b = OpBrand::defaults();
        assert_eq!(b.name, "Rustango");
        assert_eq!(b.theme_mode, "auto");
        assert!(b.tagline.is_none());
        assert!(b.primary_color.is_none());
        assert_eq!(b.logo_url, "/__static__/rustango.png");
    }

    /// `BrandSettings` overrides the defaults — but the function
    /// stays pure (no env reads), so the test doesn't need to
    /// poke `std::env::set_var` (forbidden by workspace lint).
    #[cfg(feature = "config")]
    #[test]
    fn apply_brand_settings_overrides_defaults() {
        let mut b = OpBrand::defaults();
        let mut s = crate::config::BrandSettings::default();
        s.name = Some("Acme Operator".into());
        s.tagline = Some("(prod)".into());
        s.primary_color = Some("#ff8800".into());
        s.theme_mode = Some("dark".into());
        OpBrand::apply_brand_settings(&mut b, &s);
        assert_eq!(b.name, "Acme Operator");
        assert_eq!(b.tagline.as_deref(), Some("(prod)"));
        assert_eq!(b.primary_color.as_deref(), Some("#ff8800"));
        assert_eq!(b.theme_mode, "dark");
    }

    /// Empty strings in TOML don't override (different from
    /// "explicitly absent" — a user typing `name = ""` almost
    /// certainly meant the default, not the empty string).
    #[cfg(feature = "config")]
    #[test]
    fn apply_brand_settings_empty_strings_skip() {
        let mut b = OpBrand::defaults();
        let original = b.name.clone();
        let mut s = crate::config::BrandSettings::default();
        s.name = Some(String::new());
        OpBrand::apply_brand_settings(&mut b, &s);
        assert_eq!(b.name, original);
    }

    /// Invalid hex colors are dropped, not propagated. Matches the
    /// `from_env` pre-#87 behavior — bad input falls through to
    /// the default.
    #[cfg(feature = "config")]
    #[test]
    fn apply_brand_settings_rejects_bad_hex() {
        let mut b = OpBrand::defaults();
        let mut s = crate::config::BrandSettings::default();
        s.primary_color = Some("not-a-color".into());
        OpBrand::apply_brand_settings(&mut b, &s);
        assert!(b.primary_color.is_none());
    }

    /// Invalid theme_mode values are dropped — the validator
    /// only accepts `auto` / `light` / `dark`.
    #[cfg(feature = "config")]
    #[test]
    fn apply_brand_settings_rejects_bad_theme_mode() {
        let mut b = OpBrand::defaults();
        let original = b.theme_mode.clone();
        let mut s = crate::config::BrandSettings::default();
        s.theme_mode = Some("midnight".into());
        OpBrand::apply_brand_settings(&mut b, &s);
        assert_eq!(b.theme_mode, original);
    }
}
