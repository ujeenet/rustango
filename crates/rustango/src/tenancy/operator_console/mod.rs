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

pub(crate) mod session;

use crate::core::Column as _;
use crate::sql::sqlx::PgPool;
use crate::sql::Fetcher;
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

pub use session::{SessionPayload, SessionSecret, SessionSecretError};
use session::{COOKIE_NAME, SESSION_TTL_SECS};

use super::auth;
use super::branding::{self, BrandAssetKind};
use super::pools::TenantPools;

const RUSTANGO_PNG: &[u8] = include_bytes!("../static/rustango.png");

#[derive(Clone)]
struct ConsoleState {
    registry: PgPool,
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
    /// Cookie domain for impersonation cookies. Set when the
    /// operator console is mounted at the apex and tenant
    /// admins live on subdomains — e.g. apex `localhost`,
    /// cookie domain `.localhost` lets a cookie set by the
    /// operator console reach `osu.localhost`. `None` keeps
    /// the cookie host-only (rare; only useful when operator
    /// console + tenant admin share a host). (#78)
    tenant_cookie_domain: Option<String>,
}

/// Operator console branding read from env at boot. Static for the
/// lifetime of the process; per-tenant branding lives on `Org`.
#[derive(Debug, Clone)]
struct OpBrand {
    name: String,
    tagline: Option<String>,
    logo_url: String,
    primary_color: Option<String>,
    theme_mode: String,
}

impl OpBrand {
    /// Read the operator console branding from env. Every value has
    /// a sensible default so this never fails — at worst the
    /// console renders with the rustango defaults.
    fn from_env() -> Self {
        let name =
            std::env::var("RUSTANGO_OPERATOR_BRAND_NAME").unwrap_or_else(|_| "Rustango".to_owned());
        let tagline = std::env::var("RUSTANGO_OPERATOR_TAGLINE")
            .ok()
            .filter(|s| !s.is_empty());
        let logo_url = std::env::var("RUSTANGO_OPERATOR_LOGO_URL")
            .unwrap_or_else(|_| "/__static__/rustango.png".to_owned());
        let primary_color = std::env::var("RUSTANGO_OPERATOR_PRIMARY_COLOR")
            .ok()
            .and_then(|v| branding::validate_hex_color(&v));
        let theme_mode = std::env::var("RUSTANGO_OPERATOR_THEME_MODE")
            .ok()
            .as_deref()
            .and_then(branding::validate_theme_mode)
            .unwrap_or("auto")
            .to_owned();
        Self {
            name,
            tagline,
            logo_url,
            primary_color,
            theme_mode,
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
pub fn router(registry: PgPool, secret: SessionSecret) -> Router {
    router_inner(
        registry,
        None,
        secret,
        branding::default_brand_storage(),
        None,
        None,
    )
}

/// Like [`router`] but also exposes `GET`/`POST /orgs/{slug}/edit`,
/// powered by the supplied `TenantPools` (cache-eviction on
/// `database_url` rotation). Production tenancy `Builder` wires this
/// path so operators don't need a redeploy + manual SQL to fix a
/// stale connection URL.
#[must_use]
pub fn router_with_pools(
    registry: PgPool,
    pools: Arc<TenantPools>,
    secret: SessionSecret,
) -> Router {
    router_inner(
        registry,
        Some(pools),
        secret,
        branding::default_brand_storage(),
        None,
        None,
    )
}

/// Like [`router_with_pools`] but also wires the
/// **operator-as-superuser tenant impersonation** flow (#78).
/// Pass the same `tenant_session_secret` your `TenantAdminBuilder`
/// uses so cookies the operator console mints will verify in the
/// tenant admin. `tenant_cookie_domain` controls the cookie's
/// `Domain` attribute — set to the apex (e.g. `.localhost` so
/// `osu.localhost` receives it) when operator console and tenant
/// admin live on different hostnames.
///
/// `Server::Builder::serve` calls this automatically since v0.27.8.
/// Custom mount points opt in by replacing `router_with_pools` with
/// this variant.
#[must_use]
pub fn router_with_impersonation(
    registry: PgPool,
    pools: Arc<TenantPools>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: SessionSecret,
    tenant_cookie_domain: Option<String>,
) -> Router {
    router_inner(
        registry,
        Some(pools),
        secret,
        brand_storage,
        Some(tenant_session_secret),
        tenant_cookie_domain,
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
    registry: PgPool,
    pools: Option<Arc<TenantPools>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
) -> Router {
    router_inner(registry, pools, secret, brand_storage, None, None)
}

fn router_inner(
    registry: PgPool,
    pools: Option<Arc<TenantPools>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: Option<SessionSecret>,
    tenant_cookie_domain: Option<String>,
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
        tenant_cookie_domain,
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
        .route("/orgs", get(orgs_list));
    if edit_enabled {
        private = private
            .route(
                "/orgs/{slug}/edit",
                get(org_edit_form).post(org_edit_submit),
            )
            .route("/orgs/{slug}/edit/branding", post(org_edit_branding));
    }
    if impersonation_enabled {
        // v0.27.8 (#78) — operator-as-superuser tenant admin login.
        // Mints a tenant-bound impersonation cookie and 302s to
        // the tenant admin's `/__admin/`. Audit-log entry recorded
        // on every mint so each session is traceable to an
        // operator id.
        private = private.route("/orgs/{slug}/impersonate", post(org_impersonate));
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
    let Some(payload) = payload else {
        return redirect_to_login(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
            .into_response();
    };
    match auth::Operator::objects()
        .where_(auth::Operator::id.eq(payload.oid))
        .fetch(&state.registry)
        .await
    {
        Ok(rows) => {
            let Some(op) = rows.into_iter().next().filter(|o| o.active) else {
                return redirect_to_login("/").into_response();
            };
            req.extensions_mut().insert(op);
            next.run(req).await
        }
        Err(e) => {
            tracing::warn!(target: "crate::tenancy::operator_console", error = %e, "registry lookup");
            (StatusCode::INTERNAL_SERVER_ERROR, "registry lookup failed").into_response()
        }
    }
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
        match auth::authenticate_operator(&state.registry, &form.username, &form.password).await {
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
    let rows: Vec<auth::Operator> = match auth::Operator::objects().fetch(&state.registry).await {
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
    let rows: Vec<super::Org> = match super::Org::objects().fetch(&state.registry).await {
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
const LOCKED_ORG_FIELDS: &[&str] = &[
    "id",
    "slug",
    "storage_mode",
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
    use crate::sql::sqlx::Row;

    // Fetch the row via the same select_one_row admin uses, so the
    // `render_value_for_input` calls below see the same column
    // shapes as the per-app admin's edit form would.
    let row = match crate::sql::sqlx::query(&format!(
        r#"SELECT * FROM "{}" WHERE "slug" = $1"#,
        super::Org::SCHEMA.table
    ))
    .bind(&slug)
    .fetch_optional(&state.registry)
    .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let Some(row) = row else {
        return (StatusCode::NOT_FOUND, format!("org `{slug}` not found")).into_response();
    };

    // Build per-field render contexts. Iterating `Org::SCHEMA.fields`
    // means new columns added to Org show up automatically — the
    // template doesn't need a manual update.
    let mut editable_rows: Vec<serde_json::Value> = Vec::new();
    let mut locked_rows: Vec<serde_json::Value> = Vec::new();
    for field in super::Org::SCHEMA.scalar_fields() {
        let prefill = render::render_value_for_input(&row, field);
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

    // Pull current logo / favicon paths off the row so the template
    // can render a small preview next to the upload control.
    let logo_path: Option<String> = row.try_get("logo_path").ok().flatten();
    let favicon_path: Option<String> = row.try_get("favicon_path").ok().flatten();
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
    Extension(_op): Extension<auth::Operator>,
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
    let existing_database_url = match crate::sql::sqlx::query_scalar::<_, Option<String>>(
        r#"SELECT "database_url" FROM "rustango_orgs" WHERE "slug" = $1"#,
    )
    .bind(&slug)
    .fetch_optional(&state.registry)
    .await
    {
        Ok(Some(v)) => v,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, format!("org `{slug}` not found")).into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
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
        .is_some_and(|new| existing_database_url.as_deref() != Some(new));

    // Build `UPDATE … SET col1 = $1, col2 = $2, … WHERE slug = $N`.
    use std::fmt::Write as _;
    let mut sql = format!(r#"UPDATE "{}" SET "#, super::Org::SCHEMA.table);
    for (i, (col, _)) in collected.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let _ = write!(sql, r#""{col}" = ${}"#, i + 1);
    }
    let _ = write!(sql, r#" WHERE "slug" = ${}"#, collected.len() + 1);

    let mut q = crate::sql::sqlx::query(&sql);
    for (_, value) in &collected {
        q = bind_sql_value(q, value);
    }
    q = q.bind(&slug);
    if let Err(e) = q.execute(&state.registry).await {
        return redirect_with_error(&slug, &format!("update failed: {e}"));
    }

    if database_url_changed {
        pools.invalidate(&slug).await;
    }

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

/// Bind one `SqlValue` onto a Postgres `sqlx::Query`. Mirrors the
/// admin's `update_submit` bind path. Kept inline here so the
/// operator console doesn't reach across module boundaries for
/// admin-internal helpers.
fn bind_sql_value<'a>(
    q: crate::sql::sqlx::query::Query<
        'a,
        crate::sql::sqlx::Postgres,
        crate::sql::sqlx::postgres::PgArguments,
    >,
    v: &crate::core::SqlValue,
) -> crate::sql::sqlx::query::Query<
    'a,
    crate::sql::sqlx::Postgres,
    crate::sql::sqlx::postgres::PgArguments,
> {
    use crate::core::SqlValue;
    match v {
        SqlValue::Null => q.bind(None::<i64>),
        SqlValue::I16(v) => q.bind(*v),
        SqlValue::I32(v) => q.bind(*v),
        SqlValue::I64(v) => q.bind(*v),
        SqlValue::F32(v) => q.bind(*v),
        SqlValue::F64(v) => q.bind(*v),
        SqlValue::Bool(v) => q.bind(*v),
        SqlValue::String(v) => q.bind(v.clone()),
        SqlValue::DateTime(v) => q.bind(*v),
        SqlValue::Date(v) => q.bind(*v),
        SqlValue::Uuid(v) => q.bind(*v),
        SqlValue::Json(v) => q.bind(crate::sql::sqlx::types::Json(v.clone())),
        SqlValue::List(_) => panic!("List not expected in op_orgs UPDATE"),
    }
}

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
    Extension(_op): Extension<auth::Operator>,
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
    // Apply all updates in one statement.
    use crate::core::Model as _;
    use std::fmt::Write as _;
    let mut sql = format!(r#"UPDATE "{}" SET "#, super::Org::SCHEMA.table);
    for (i, (col, _)) in updates.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let _ = write!(sql, r#""{col}" = ${}"#, i + 1);
    }
    let _ = write!(sql, r#" WHERE "slug" = ${}"#, updates.len() + 1);
    let mut q = crate::sql::sqlx::query(&sql);
    for (_, v) in &updates {
        q = q.bind(v.clone());
    }
    q = q.bind(&slug);
    if let Err(e) = q.execute(&state.registry).await {
        return redirect_with_error(&slug, &format!("update failed: {e}"));
    }
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
// `/orgs/{slug}/edit`. Mints a tenant-bound `TenantSessionPayload`
// with `imp = Some(operator_id)`, sets it as the tenant cookie on
// the apex domain (so the subdomain receives it), and 302s to the
// tenant admin's `/__admin/`. The tenant admin's `validate_session`
// branches on `payload.imp.is_some()` and treats the request as
// superuser. Banner + audit-log entries make impersonation visible
// + traceable.

async fn org_impersonate(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
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
        .fetch(&state.registry)
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
    let ttl = std::env::var("RUSTANGO_OPERATOR_IMPERSONATION_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(super::tenant_console::IMPERSONATION_TTL_SECS);
    let payload =
        super::tenant_console::TenantSessionPayload::impersonation(operator_id, slug.clone(), ttl);
    let cookie_value = super::tenant_console::encode(&tenant_secret, &payload);

    // Audit-log entry on the operator side. The tenant admin
    // emits a separate entry the first time an impersonation
    // session lands on a write — both ends are visible.
    let audit_sql = r#"
        INSERT INTO "rustango_audit_log"
            ("entity_table", "entity_pk", "operation", "source", "changes")
        VALUES ('rustango_orgs', $1, 'action', $2,
                jsonb_build_object('action', 'impersonate.start',
                                   'tenant_slug', $1::text,
                                   'operator_id', $3::bigint))
    "#;
    if let Err(e) = rustango::sql::sqlx::query(audit_sql)
        .bind(&slug)
        .bind(format!("operator:{operator_id}:impersonating"))
        .bind(operator_id)
        .execute(&state.registry)
        .await
    {
        tracing::warn!(
            target: "crate::tenancy::operator_console",
            error = %e,
            slug = %slug,
            operator_id,
            "failed to record impersonation start in audit log",
        );
        // Continue anyway — audit failure must not block the
        // operator's primary workflow. The tracing log line is
        // the fallback record.
    }

    // Build the redirect URL. We send the operator to the
    // tenant subdomain root which the tenant admin's session
    // middleware re-redirects to /__admin/ when authenticated.
    // Apex schema-aware: respect `RUSTANGO_TENANT_SCHEME`
    // (defaults to http for local dev; ops set https in prod).
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
    let port_suffix = std::env::var("RUSTANGO_TENANT_PORT")
        .ok()
        .filter(|s| !s.is_empty() && s != "80" && s != "443")
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let redirect_to = format!("{scheme}://{host}{port_suffix}/__admin/");

    // Build the cookie. `Domain=` so subdomains receive it;
    // `Path=/` so all admin routes pick it up.
    let mut cookie = Cookie::build((super::tenant_console::COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(ttl))
        .build();
    if let Some(domain) = state.tenant_cookie_domain.as_deref() {
        cookie.set_domain(domain.to_owned());
    }
    let mut resp = Redirect::to(&redirect_to).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie.to_string()) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    tracing::info!(
        target: "crate::tenancy::operator_console",
        slug = %slug,
        operator_id,
        ttl_secs = ttl,
        redirect_to = %redirect_to,
        "minted impersonation cookie",
    );
    resp
}
