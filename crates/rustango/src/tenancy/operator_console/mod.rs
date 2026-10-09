//! Operator-facing admin console: form login plus a sidebar layout for
//! operator routes.
//!
//! It is kept separate from `rustango-admin` so the operator UI can
//! change without touching the per-tenant admin look.
//!
//! ## Logging
//!
//! Two `tracing` streams. The subscriber the app installs decides the
//! format.
//!
//! **Actions** — one event per mutation, on target [`ACTION_TARGET`]:
//!
//! ```text
//! event=operator_action action=host_add operator_id=1
//!   entity=rustango_orgs entity_id=acme fields=hostname,tenant_slug
//! ```
//!
//! The same function emits the event and writes the audit row, so the
//! log and the durable trail agree. If the row fails to write you get an
//! `ERROR` with `event=operator_action_unrecorded`: the action happened
//! but was not recorded. `fields` lists the *names* that changed, never
//! the values, because logs travel further than the database does.
//!
//! **Requests** — one event per request from [`crate::access_log`]
//! (method, path, status, duration, IP), with `next` and `token`
//! redacted out of query strings.
//!
//! Filter console activity by target:
//!
//! ```text
//! RUST_LOG=warn,rustango::tenancy::operator_console::action=info
//! ```
//!
//! ## Routes
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
//! Creating tenants and operators still runs through the `Cli` verbs:
//! they do work (CREATE SCHEMA, migrations, password hashing) that does
//! not fit one HTTP form. The edit routes cover the knobs an operator
//! changes live: display name, host pattern, port, path prefix, active
//! flag, and `database_url` for database-mode tenants. Only
//! `database_url` rebuilds a pool — see [`org_edit_submit`].
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

/// Tenant creation from the console, and its progress view.
/// Mounted only by [`router_with_provisioning`].
mod audit;
mod decommission;
mod hosts;
mod migrate;
mod mount;
mod operators;
mod provisioning;

use crate::core::Column as _;
// `ConsoleState.registry` is `crate::sql::Pool`, the backend-erasing enum,
// so every query here works on any backend.
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

// `session` (HMAC-signed cookies, SessionSecret) lives one level up at
// `tenancy::session` so other callers can reach it too.
pub use super::session::{self, SessionPayload, SessionSecret, SessionSecretError};
use super::session::{COOKIE_NAME, SESSION_TTL_SECS};

use super::auth;
use super::branding::{self, BrandAssetKind};
#[allow(unused_imports)] // referenced only by rustdoc links
use super::pools::TenantPools;

const RUSTANGO_PNG: &[u8] = include_bytes!("../static/rustango.png");

/// Tracing target for operator actions.
///
/// It has its own target so a deployment can route or silence console
/// activity on its own:
/// `RUST_LOG=rustango::tenancy::operator_console::action=info`.
/// Request logs come from [`crate::access_log`] under a separate
/// `rustango::access_log` target.
pub const ACTION_TARGET: &str = "rustango::tenancy::operator_console::action";

#[derive(Clone)]
struct ConsoleState {
    /// Backend-erasing registry pool, so one code path serves
    /// PG / MySQL / SQLite.
    registry: crate::sql::Pool,
    /// Pool cache. When `Some`, the `/orgs/{slug}/edit` routes are
    /// mounted: rotating `database_url` must drop that org's cached
    /// `TenantPool` so the next request rebuilds against the new URL.
    /// With `None` (the [`router`] entry point) the console is read-only.
    pools: Option<Arc<dyn crate::tenancy::TenantPoolInvalidator>>,
    /// When `Some`, the console can **create** tenants — see
    /// [`provisioning`]. Kept apart from `pools` because creating a
    /// tenant takes a database URL and connects to it, so a deployment
    /// opts in through [`router_with_provisioning`] instead of getting
    /// it along with the edit routes.
    provisioner: Option<Arc<dyn crate::tenancy::provision::TenantProvisioner>>,
    session_secret: Arc<SessionSecret>,
    tera: Arc<Tera>,
    /// Storage for per-tenant brand assets (logo, favicon). Defaults to
    /// `LocalStorage` under `./var/brand`; set `RUSTANGO_BRAND_STORAGE_DIR`
    /// to change it.
    brand_storage: BoxedStorage,
    /// The console's own brand, read from env at boot and put into every
    /// render context so a deployment can rebrand without editing
    /// templates.
    op_brand: Arc<OpBrand>,
    /// Tenant-side session secret. When `Some`, the console exposes
    /// `POST /orgs/{slug}/impersonate`, which mints a tenant-bound
    /// `TenantSessionPayload` with `imp = Some(operator_id)` so an
    /// operator can open the tenant admin without a tenant password.
    /// `Server::Builder::serve` wires it; other mount points use the
    /// `_with_impersonation` constructors.
    tenant_session_secret: Option<Arc<SessionSecret>>,
    /// URL on the tenant admin where impersonation lands the browser to
    /// redeem a signed handoff token. Mirrors
    /// [`super::routes::RouteConfig::impersonation_handoff_url`].
    /// Default `/_impersonation_handoff`.
    tenant_handoff_url: String,
}

/// The bundled logo, served by the console itself.
const DEFAULT_LOGO_URL: &str = "/__static__/rustango.png";

/// Operator console branding, resolved once at boot. Per-tenant
/// branding lives on `Org` instead.
///
/// Highest priority first:
/// 1. `RUSTANGO_OPERATOR_*` env vars
/// 2. `[brand]` in `config/<env>_settings.toml`
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
            logo_url: DEFAULT_LOGO_URL.to_owned(),
            primary_color: None,
            theme_mode: "auto".to_owned(),
        }
    }

    /// Resolve console branding: defaults, then `Settings.brand` from
    /// TOML, then env vars, so an env override needs no config push.
    /// A missing config file is skipped without error.
    fn from_env() -> Self {
        let mut out = Self::defaults();
        #[cfg(feature = "config")]
        match crate::config::Settings::load_from_env() {
            Ok(s) => Self::apply_brand_settings(&mut out, &s.brand),
            Err(e) if e.is_missing_config() => {}
            // A broken config should say so, not silently drop the brand (#1948).
            Err(e) => warn_config_error(&e),
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

/// [`router_with_pools`] plus the routes that **create** tenants:
/// `GET`/`POST /orgs/new`, `POST /orgs/test-connection`, and the
/// provisioning run view with its SSE stream.
///
/// It is a separate constructor, not a flag, because creating a tenant
/// takes a database URL from a form and connects to it. A deployment
/// says yes to that on purpose.
///
/// Build the provisioner with
/// [`crate::tenancy::provision::Provisioner::new`], which closes over
/// the pools, the registry URL and the migrations directory:
///
/// ```ignore
/// let provisioner = tenancy::provision::Provisioner::new(
///     pools.clone(), registry_url.clone(), "migrations",
/// ).erased();
/// let console = operator_console::router_with_provisioning(
///     registry, pools.into_invalidator(), provisioner, secret,
/// );
/// ```
///
/// ## Authorization
///
/// Any operator who can reach the console can use these routes. There
/// is no per-operator permission model; the choice of router *is* the
/// authorization. [`router`] is read-only, [`router_with_pools`] can
/// edit, this one can also create tenants.
#[must_use]
pub fn router_with_provisioning(
    registry: impl Into<crate::sql::Pool>,
    pools: Arc<dyn crate::tenancy::TenantPoolInvalidator>,
    provisioner: Arc<dyn crate::tenancy::provision::TenantProvisioner>,
    secret: SessionSecret,
) -> Router {
    router_inner(
        registry.into(),
        Some(pools),
        Some(provisioner),
        secret,
        branding::default_brand_storage(),
        None,
        default_tenant_handoff_url(),
    )
}

/// Build the read-only operator-console `axum::Router`. Mount it at the
/// apex host, at the root or nested under a path prefix.
///
/// Use [`router_with_pools`] to let operators edit org config (display
/// name, host pattern, port, path prefix, active flag, `database_url`)
/// from the UI. That variant needs a [`TenantPools`] handle so it can
/// evict the cached pool when `database_url` changes.
///
/// Brand uploads go to [`branding::default_brand_storage`]. For S3, R2,
/// B2, MinIO or a CDN-fronted bucket, use [`router_with_brand_storage`].
#[must_use]
pub fn router(registry: impl Into<crate::sql::Pool>, secret: SessionSecret) -> Router {
    router_inner(
        registry.into(),
        None,
        None,
        secret,
        branding::default_brand_storage(),
        None,
        default_tenant_handoff_url(),
    )
}

/// Like [`router`], but also exposes `GET`/`POST /orgs/{slug}/edit`.
/// The supplied pools handle evicts the cached pool when `database_url`
/// changes, so fixing a stale connection URL needs no redeploy.
#[must_use]
pub fn router_with_pools(
    registry: impl Into<crate::sql::Pool>,
    pools: Arc<dyn crate::tenancy::TenantPoolInvalidator>,
    secret: SessionSecret,
) -> Router {
    router_inner(
        registry.into(),
        Some(pools),
        None,
        secret,
        branding::default_brand_storage(),
        None,
        default_tenant_handoff_url(),
    )
}

/// Like [`router_with_pools`], plus operator-as-superuser tenant
/// impersonation. Pass the same `tenant_session_secret` your
/// `TenantAdminBuilder` uses, or the handoff token will not verify.
///
/// The handoff goes through a URL token, not a cookie: the console
/// mints a signed token and redirects to
/// `<sub>.<apex><tenant_handoff_url>?token=<...>`; the tenant admin
/// redeems it and sets a host-scoped cookie. No impersonation cookie is
/// set on the operator-console origin.
///
/// `Server::Builder::serve` calls this for you. Other mount points swap
/// `router_with_pools` for this variant.
#[must_use]
pub fn router_with_impersonation(
    registry: impl Into<crate::sql::Pool>,
    pools: Arc<dyn crate::tenancy::TenantPoolInvalidator>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: SessionSecret,
    tenant_handoff_url: String,
) -> Router {
    router_inner(
        registry.into(),
        Some(pools),
        None,
        secret,
        brand_storage,
        Some(tenant_session_secret),
        tenant_handoff_url,
    )
}

/// Like [`router`], but takes any [`BoxedStorage`] for brand assets:
/// `S3Storage` (AWS, R2, B2, MinIO), a `LocalStorage` with
/// `with_base_url` behind a CDN, or your own `Storage` impl.
///
/// If the backend returns a URL from `Storage::url`, `<img src>` points
/// straight at it. Otherwise the built-in
/// `/__brand__/{slug}/{filename}` handler serves the file.
///
/// `pools = Some(...)` mounts the org-edit and brand-upload routes;
/// `None` keeps the console read-only.
#[must_use]
pub fn router_with_brand_storage(
    registry: impl Into<crate::sql::Pool>,
    pools: Option<Arc<dyn crate::tenancy::TenantPoolInvalidator>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
) -> Router {
    router_inner(
        registry.into(),
        pools,
        None,
        secret,
        brand_storage,
        None,
        default_tenant_handoff_url(),
    )
}

/// Handoff URL used by the constructors that do not take one.
/// Same value as [`super::routes::RouteConfig`]'s default.
/// A logged `500` whose body withholds the cause (#1955).
fn server_error(context: &str, e: &dyn std::fmt::Display) -> Response<Body> {
    let body = crate::error::server_error_body(context, e);
    (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
}

/// Operator text for a failure whose cause is logged, not shown (#2034).
/// The log line names the org, so the operator can find it.
fn withheld(context: &str, slug: &str, what: &str, e: &dyn std::fmt::Display) -> String {
    withheld_in(
        &tracing::error_span!("operator_console", org = %slug),
        context,
        what,
        e,
    )
}

/// [`withheld`] logged under `span`, for a failure that is not about one org.
pub(crate) fn withheld_in(
    span: &tracing::Span,
    context: &str,
    what: &str,
    e: &dyn std::fmt::Display,
) -> String {
    let body = span.in_scope(|| crate::error::server_error_body(context, e));
    format!("{what} ({body})")
}

fn default_tenant_handoff_url() -> String {
    super::routes::RouteConfig::default().impersonation_handoff_url
}

/// Every knob at once. The named constructors above are shorthands
/// over this one; use it for a combination they do not cover, such as
/// impersonation **and** provisioning together.
///
/// `pools` unlocks the edit routes, `provisioner` the create routes and
/// `tenant_session_secret` impersonation. `None` leaves those routes
/// unmounted.
#[must_use]
pub fn router_full(
    registry: impl Into<crate::sql::Pool>,
    pools: Option<Arc<dyn crate::tenancy::TenantPoolInvalidator>>,
    provisioner: Option<Arc<dyn crate::tenancy::provision::TenantProvisioner>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: Option<SessionSecret>,
    tenant_handoff_url: String,
) -> Router {
    router_inner(
        registry.into(),
        pools,
        provisioner,
        secret,
        brand_storage,
        tenant_session_secret,
        tenant_handoff_url,
    )
}

/// [`router_full`] without its own access log, for `server::Builder` when
/// it mounts observability itself; one line per request (#1788).
pub(crate) fn router_full_unlogged(
    registry: crate::sql::Pool,
    pools: Option<Arc<dyn crate::tenancy::TenantPoolInvalidator>>,
    provisioner: Option<Arc<dyn crate::tenancy::provision::TenantProvisioner>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: Option<SessionSecret>,
    tenant_handoff_url: String,
) -> Router {
    router_unlogged(
        registry,
        pools,
        provisioner,
        secret,
        brand_storage,
        tenant_session_secret,
        tenant_handoff_url,
    )
}

fn router_inner(
    registry: crate::sql::Pool,
    pools: Option<Arc<dyn crate::tenancy::TenantPoolInvalidator>>,
    provisioner: Option<Arc<dyn crate::tenancy::provision::TenantProvisioner>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: Option<SessionSecret>,
    tenant_handoff_url: String,
) -> Router {
    use crate::access_log::AccessLogRouterExt as _;
    router_unlogged(
        registry,
        pools,
        provisioner,
        secret,
        brand_storage,
        tenant_session_secret,
        tenant_handoff_url,
    )
    .access_log(access_log_layer())
}

/// The console's own access log. `next` is redacted because the login
/// bounce carries the attempted URL, `token` for the impersonation handoff.
pub(crate) fn access_log_layer() -> crate::access_log::AccessLogLayer {
    crate::access_log::AccessLogLayer::new()
        .redact_additional("next")
        .redact_additional("token")
}

fn router_unlogged(
    registry: crate::sql::Pool,
    pools: Option<Arc<dyn crate::tenancy::TenantPoolInvalidator>>,
    provisioner: Option<Arc<dyn crate::tenancy::provision::TenantProvisioner>>,
    secret: SessionSecret,
    brand_storage: BoxedStorage,
    tenant_session_secret: Option<SessionSecret>,
    tenant_handoff_url: String,
) -> Router {
    let mut tera = crate::template_extensions::html_tera();
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
            "_op_pager.html",
            include_str!("../templates/_op_pager.html"),
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
            "op_org_hosts.html",
            include_str!("../templates/op_org_hosts.html"),
        ),
        (
            "op_change_password.html",
            include_str!("../templates/op_change_password.html"),
        ),
        (
            "op_sso_shared.html",
            include_str!("../templates/op_sso_shared.html"),
        ),
        (
            "op_orgs_new.html",
            include_str!("../templates/op_orgs_new.html"),
        ),
        (
            "op_provision_run.html",
            include_str!("../templates/op_provision_run.html"),
        ),
        (
            "op_provision_runs.html",
            include_str!("../templates/op_provision_runs.html"),
        ),
        ("op_audit.html", include_str!("../templates/op_audit.html")),
    ])
    .expect("operator-console templates parse");
    let edit_enabled = pools.is_some();
    let impersonation_enabled = tenant_session_secret.is_some() && pools.is_some();
    let provisioning_enabled = provisioner.is_some();
    let state = ConsoleState {
        registry,
        pools,
        provisioner,
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
        .route("/logout", post(logout));
    // Cacheable GET-only assets stay outside the CSRF layers, so no
    // shared cache stores a `Set-Cookie` with them.
    let assets = Router::new()
        .route("/__static__/rustango.png", get(static_rustango_png))
        .route("/__brand__/{slug}/{filename}", get(serve_brand_asset));

    // Authenticated routes: the middleware injects an `Extension<auth::Operator>`.
    let mut private = Router::new()
        .route("/", get(welcome))
        .route("/operators", get(operators::operators_list))
        .route("/orgs", get(orgs_list))
        // Read-only, so it is not behind the edit gate: a read-only
        // console is exactly where "what happened?" still needs
        // answering, and every operator can already see everything the
        // log describes.
        .route("/audit", get(audit::audit_list))
        .route(
            "/change-password",
            get(change_password_form).post(change_password_submit),
        );
    // Registry-wide shared SSO providers (admin-sso): every operator sees
    // the list; changing it needs an editable console.
    #[cfg(feature = "admin-sso")]
    {
        private = private.route("/sso-shared", get(sso_shared_list));
        if edit_enabled {
            private = private
                .route("/sso-shared", post(sso_shared_create))
                .route("/sso-shared/{id}/delete", post(sso_shared_delete))
                .route(
                    "/sso-shared/{id}/email-link",
                    post(sso_shared_set_email_link),
                );
        }
    }
    if edit_enabled {
        private = private
            // Who can sign in to this console. `edit_enabled` is really
            // the switch between a read-only console and one that can
            // change things, and creating an operator is a change.
            .route("/operators", post(operators::operator_create))
            // Open every tenant pool up front, e.g. after a deploy or a
            // credential rotation. Gated on pools, not the provisioner:
            // warming needs no migrations directory.
            .route(
                "/orgs/prewarm",
                get(orgs_post_only_redirect).post(prewarm_pools),
            )
            .route(
                "/operators/{id}/active",
                get(op_post_only_redirect).post(operators::operator_set_active),
            )
            .route(
                "/operators/{id}/reset-password",
                get(op_post_only_redirect).post(operators::operator_reset_password),
            )
            .route(
                "/orgs/{slug}/edit",
                get(org_edit_form).post(org_edit_submit),
            )
            // POST takes the multipart upload; GET bounces back to the
            // edit form so a manual URL hit does not 405.
            .route(
                "/orgs/{slug}/edit/branding",
                get(org_post_only_redirect).post(org_edit_branding),
            )
            // Taking a tenant out of service. Behind the edit gate:
            // `purge` is the most destructive thing this console can
            // do, and a read-only console must not offer it.
            .route(
                "/orgs/{slug}/deactivate",
                get(org_post_only_redirect).post(decommission::deactivate),
            )
            .route(
                "/orgs/{slug}/purge",
                get(org_post_only_redirect).post(decommission::purge),
            )
            // Probing an existing tenant, as opposed to the create
            // form's probe of a URL being typed.
            .route(
                "/orgs/{slug}/test-connection",
                post(provisioning::test_tenant_connection),
            )
            // Extra hostnames a tenant answers on. Binding a hostname
            // changes which tenant serves that traffic, so it sits
            // behind the edit gate too. Three routes, not one submit,
            // because each acts on a single row.
            .route("/orgs/{slug}/hosts", get(hosts::org_hosts_view))
            .route(
                "/orgs/{slug}/hosts/add",
                get(org_post_only_redirect).post(hosts::org_hosts_add),
            )
            .route(
                "/orgs/{slug}/hosts/remove",
                get(org_post_only_redirect).post(hosts::org_hosts_remove),
            )
            .route(
                "/orgs/{slug}/hosts/toggle",
                get(org_post_only_redirect).post(hosts::org_hosts_toggle),
            );
    }
    if provisioning_enabled {
        // Creating a tenant, and watching it happen. Mounted only when
        // the deployment supplied a provisioner, because this takes a
        // database URL and connects to it.
        private = private
            .route(
                "/orgs/new",
                get(provisioning::org_new_form).post(provisioning::org_new_submit),
            )
            .route("/orgs/test-connection", post(provisioning::test_connection))
            // Migrations ride with provisioning: both need the
            // provisioner's migrations directory and its registry URL.
            .route(
                "/orgs/migrate",
                get(op_post_only_redirect).post(migrate::migrate_all),
            )
            .route(
                "/orgs/{slug}/migrate",
                get(org_post_only_redirect).post(migrate::migrate_one),
            )
            // The index has to come before the `{run_id}` route it
            // shares a prefix with, and be a distinct path: a run id is
            // numeric, so `/orgs/provision` cannot be confused for one.
            .route("/orgs/provision", get(provisioning::provision_runs_index))
            .route(
                "/orgs/provision/{run_id}",
                get(provisioning::provision_run_view),
            )
            .route(
                "/orgs/provision/{run_id}/stream",
                get(provisioning::provision_run_stream),
            );
    }
    if impersonation_enabled {
        // Operator-as-superuser tenant admin login: mint a handoff
        // token and redirect to the tenant admin. Every mint writes an
        // audit row, so a session traces back to an operator id.
        // GET bounces back, as with branding above.
        private = private.route(
            "/orgs/{slug}/impersonate",
            get(org_post_only_redirect).post(org_impersonate),
        );
    }
    let private = private.route_layer(middleware::from_fn_with_state(
        state.clone(),
        require_session,
    ));

    // CSRF on every console POST, login included (#1710): a tenant
    // subdomain is same-site with the apex, so `SameSite=Lax` does not
    // stop it. The layer checks token and Origin first; inside it,
    // `csrf_context` mints the token `render` puts in each form and sets
    // its cookie, so the layer sees it set and adds no second one.
    // Origin is the check that stops a tenant host: it can plant the
    // cookie, and so knows the token.
    public
        .merge(private)
        .route_layer(middleware::from_fn(
            crate::admin::csrf_context::csrf_context,
        ))
        .route_layer(crate::forms::csrf::layer())
        .merge(assets)
        .with_state(state)
        .layer(middleware::from_fn(mount::layer))
}

/// Rows per page, for every list the console renders. One number for
/// all of them, so a deployment with three tenants and one with
/// thousands behave the same way.
pub(super) const PAGE_SIZE: usize = 50;

/// One page of a console list.
///
/// A thin adapter over [`crate::pagination::Paginator`]: it counts the
/// rows, asks the paginator for the page, and flattens the result into
/// template context.
///
/// Use it instead of doing the arithmetic per view. A huge `?page=`
/// overflows a naive `(page - 1) * size`; `Paginator::get_page` clamps
/// instead.
pub(super) struct Paged {
    pub(super) offset: i64,
    pub(super) limit: i64,
    number: usize,
    total: usize,
    num_pages: usize,
    has_previous: bool,
    has_next: bool,
    marks: Vec<serde_json::Value>,
}

impl Paged {
    /// From a total the caller already knows — a filtered count, say.
    pub(super) fn from_total(total: i64, requested: Option<i64>) -> Self {
        let total = usize::try_from(total).unwrap_or(0);
        let paginator = crate::pagination::Paginator::new(total, PAGE_SIZE);
        // `get_page` clamps rather than erroring: a page number out of
        // range is a stale link, not something to show a 500 for.
        let page = paginator.get_page(requested.unwrap_or(1));
        let marks = paginator
            .get_elided_page_range(page.number, 3, 2)
            .into_iter()
            .map(|mark| match mark {
                crate::pagination::PageMark::Number(n) => serde_json::json!({"number": n}),
                crate::pagination::PageMark::Ellipsis => serde_json::json!({"ellipsis": true}),
            })
            .collect();
        Self {
            offset: i64::try_from(page.offset()).unwrap_or(i64::MAX),
            limit: i64::try_from(page.limit()).unwrap_or(i64::MAX),
            number: page.number,
            total,
            num_pages: paginator.num_pages(),
            has_previous: page.has_previous(),
            has_next: page.has_next(),
            marks,
        }
    }

    /// Counting the model's rows first, for the lists with no filter.
    pub(super) async fn of_model(
        pool: &crate::sql::Pool,
        model: &'static crate::core::ModelSchema,
        requested: Option<i64>,
    ) -> Result<Self, crate::sql::ExecError> {
        let total = count_where(pool, model, crate::core::WhereExpr::default()).await?;
        Ok(Self::from_total(total, requested))
    }

    /// What `_op_pager.html` needs.
    ///
    /// Key names match `template_views::ListView`'s so one partial
    /// renders both. `base` is the path the links point at; `suffix`
    /// carries active filters so paging does not drop them.
    pub(super) fn inject(&self, ctx: &mut Context, base: &str, suffix: &str) {
        ctx.insert("page", &self.number);
        ctx.insert("total_pages", &self.num_pages);
        ctx.insert("total", &self.total);
        ctx.insert("has_prev", &self.has_previous);
        ctx.insert("has_next", &self.has_next);
        ctx.insert("page_marks", &self.marks);
        ctx.insert("pager_base", base);
        ctx.insert("query_suffix", suffix);
    }
}

/// Which nav entry to highlight for a path.
///
/// Only the generic-view path needs this; the hand-written handlers
/// set `section` themselves.
#[cfg(feature = "template_views")]
fn nav_section(path: &str) -> &'static str {
    match path.split('/').nth(1).unwrap_or("") {
        "orgs" => "orgs",
        "operators" => "operators",
        "audit" => "audit",
        "sso-shared" => "sso",
        "change-password" => "change_password",
        _ => "home",
    }
}

/// `SELECT COUNT(*)` over one model, optionally filtered.
///
/// Separate from [`Paged`] because a list view sometimes needs a count
/// that is *not* its page count — the operator list pages its rows but
/// still has to know how many operators are active in total, which a
/// page of rows cannot tell it.
pub(super) async fn count_where(
    pool: &crate::sql::Pool,
    model: &'static crate::core::ModelSchema,
    where_clause: crate::core::WhereExpr,
) -> Result<i64, crate::sql::ExecError> {
    let count = crate::core::CountQuery::new(model, where_clause);
    crate::sql::count_rows_pool(pool, &count).await
}

/// The `?page=` parameter, for the list views that take nothing else.
#[derive(Deserialize)]
pub(super) struct PageQuery {
    #[serde(default)]
    pub(super) page: Option<i64>,
}

/// A page plus the outcome of whatever redirected here.
#[derive(Deserialize)]
pub(super) struct ListQuery {
    #[serde(default)]
    pub(super) page: Option<i64>,
    #[serde(default)]
    pub(super) error: Option<String>,
    #[serde(default)]
    pub(super) notice: Option<String>,
}

/// Render a template, or return a 500 that names it.
///
/// Prefer this to `.unwrap_or_default()`, which turns a template error
/// into a blank `200` with no clue what went wrong.
fn render(state: &ConsoleState, template: &str, ctx: &Context) -> Response<Body> {
    // Every console form posts this back; `csrf::layer()` checks it (#1710).
    let mut ctx = ctx.clone();
    ctx.insert(
        "csp_nonce",
        &crate::csp_nonce::current().unwrap_or_default(),
    );
    // `csrf_token` is for scripts that send `X-CSRF-Token` (fetch, and
    // the multipart branding form, whose body the layer cannot read).
    if let Some(token) = crate::admin::session::current_csrf_token() {
        ctx.insert("csrf_input", &crate::forms::csrf::csrf_input_html(&token));
        ctx.insert("csrf_token", &token);
    }
    match state.tera.render(template, &ctx) {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            let mut detail = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                detail.push_str(": ");
                detail.push_str(&s.to_string());
                source = s.source();
            }
            server_error(
                "operator_console::render",
                &format!("could not render {template}: {detail}"),
            )
        }
    }
}

/// Put the console branding keys into a render context. Doing it in
/// one place keeps `op_layout.html` and `op_login.html` in sync.
fn inject_op_brand(ctx: &mut Context, brand: &OpBrand) {
    // Show the "Shared SSO" nav entry only when the admin-sso feature is
    // compiled in (its routes exist only then).
    ctx.insert("sso_console", &cfg!(feature = "admin-sso"));
    // Templates prefix every console link with it (#2007).
    let prefix = mount::MountPrefix::current();
    ctx.insert("console_prefix", prefix.as_str());
    ctx.insert("console_home", &prefix.url("/"));
    ctx.insert("brand_name", &brand.name);
    ctx.insert("brand_tagline", &brand.tagline);
    // A configured logo URL is the deployment's own; only ours moves.
    let logo_url = if brand.logo_url == DEFAULT_LOGO_URL {
        prefix.url(DEFAULT_LOGO_URL)
    } else {
        brand.logo_url.clone()
    };
    ctx.insert("brand_logo_url", &logo_url);
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
    let payload = crate::cookies::cookie_from_headers(&headers, COOKIE_NAME)
        .and_then(|v| session::decode(&state.session_secret, v).ok());
    // A 303 back from login turns the original POST into a GET, which
    // then 405s on POST-only routes. Trim `next` down to the parent GET
    // URL first. Unsaved form data is lost either way, but the operator
    // does not land on a 405 page.
    let method = req.method().clone();
    let raw_next = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let safe_next = sanitize_next_for_method(&method, raw_next);
    let Some(payload) = payload else {
        return redirect_to_login(&safe_next).into_response();
    };
    match auth::Operator::objects()
        .where_(auth::Operator::id.eq(payload.oid))
        .fetch(&state.registry)
        .await
    {
        Ok(rows) => {
            let Some(op) = rows.into_iter().next().filter(|o| o.active) else {
                return redirect_to_login(&safe_next).into_response();
            };
            // Drop sessions issued before the last password change or logout.
            if !session::session_survives(
                &state.session_secret,
                &payload.pwf,
                payload.iat,
                &op.password_hash,
                op.password_changed_at,
                op.sessions_revoked_at,
            ) {
                return redirect_to_login(&safe_next).into_response();
            }
            // Chrome for `op_layout.html`, so a generic view mounted
            // here renders with the console layout. Gated because
            // `tenancy` does not depend on `template_views`.
            #[cfg(feature = "template_views")]
            {
                let mut chrome = Context::new();
                inject_op_brand(&mut chrome, &state.op_brand);
                chrome.insert("operator_username", &op.username);
                chrome.insert("section", &nav_section(uri.path()));
                chrome.insert("edit_enabled", &state.pools.is_some());
                chrome.insert("provisioning_enabled", &state.provisioner.is_some());
                req.extensions_mut()
                    .insert(crate::template_views::ExtraContext(chrome));
            }
            req.extensions_mut().insert(op);
            next.run(req).await
        }
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::operator_console", error = %e, "registry lookup");
            (StatusCode::INTERNAL_SERVER_ERROR, "registry lookup failed").into_response()
        }
    }
}

/// GET handler for POST-only sub-form routes. It bounces back to the
/// parent edit form, so a bare GET lands somewhere useful instead of
/// on a 405 page.
async fn org_post_only_redirect(
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Redirect {
    Redirect::to(&format!("/orgs/{slug}/edit"))
}

/// The same bounce for the operator routes, which are keyed by id
/// rather than slug and land back on the one list page.
async fn op_post_only_redirect() -> Redirect {
    Redirect::to("/operators")
}

async fn orgs_post_only_redirect() -> Redirect {
    Redirect::to("/orgs")
}

/// Open a pool for every active database-mode tenant.
///
/// Run it after a deploy, a registry restart or a credential rotation.
/// It moves the connect cost off the first request to each tenant, and
/// it shows an unreachable tenant before a user finds it.
async fn prewarm_pools(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    let Some(pools) = state.pools.as_ref() else {
        return orgs_notice("pool management is not available on this console", true);
    };
    let report = match pools.prewarm().await {
        Ok(r) => r,
        Err(e) => {
            let msg = withheld_in(
                &tracing::error_span!("operator_console", operator = %op.username),
                "operator_console::prewarm",
                "Pre-warm failed",
                &e,
            );
            return orgs_notice(&msg, true);
        }
    };

    let mut detail = serde_json::Map::new();
    detail.insert("action".into(), serde_json::json!("pools.prewarm"));
    detail.insert("warmed".into(), serde_json::json!(report.warmed));
    detail.insert("failed".into(), serde_json::json!(report.failed));
    emit_registry_audit(
        &state.registry,
        "rustango_orgs",
        "*",
        op.id.get().copied().unwrap_or(0),
        "prewarm",
        detail,
    )
    .await;

    // A failure here is per-tenant and already logged by the pool layer;
    // say how many so the operator knows to go looking.
    let mut msg = format!(
        "warmed {} of {} active database-mode tenant(s)",
        report.warmed, report.total_active
    );
    if report.failed > 0 {
        msg.push_str(&format!(" — {} failed, see the logs", report.failed));
    }
    if report.skipped_cap > 0 {
        msg.push_str(&format!(
            " — {} skipped, the pool cache is at its cap",
            report.skipped_cap
        ));
    }
    orgs_notice(&msg, report.failed > 0)
}

/// PRG back to the tenant list with a message.
fn orgs_notice(msg: &str, is_error: bool) -> Response<Body> {
    let key = if is_error { "error" } else { "notice" };
    Redirect::to(&format!("/orgs?{key}={}", urlencoding_lite(msg))).into_response()
}

/// Rewrite a `next` path so the bounce back from `/login` lands on a
/// URL that answers GET.
///
/// GET and HEAD keep the path as-is. Anything else strips back to the
/// nearest known parent with a GET handler, or to `/` when the path is
/// not recognised.
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
) -> Response<Body> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("next", &q.next.unwrap_or_else(|| "/".into()));
    ctx.insert("error", &q.error);
    render(&state, "op_login.html", &ctx)
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
    ip: crate::login_throttle::ClientIp,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginSubmit>,
) -> Response<Body> {
    use crate::login_throttle::{LoginRefused, LoginScope};
    use crate::signals::auth::{
        meta_from_parts, send_user_logged_in, send_user_login_failed, AuthFailureReason,
        UserLoggedInContext, UserLoginFailedContext,
    };
    let meta = meta_from_parts(&extensions, &headers, Some("/login"));
    let next = sanitize_next(form.next.as_deref());

    // Rate limits and the account lock, before the lookup (#1609).
    let mut attempt = match crate::login_throttle::shared()
        .begin(&LoginScope::Operator, &ip, &form.username)
        .await
    {
        Ok(a) => a,
        Err(refused) => return refused.into_response(),
    };
    let found = match auth::find_operator(&state.registry, &form.username).await {
        Ok(op) => op,
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::operator_console", error = %e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };
    if let Some(op) = &found {
        if let Err(refused) = attempt.resolve(&op.username).await {
            return refused.into_response();
        }
    }

    let principal = match auth::check_operator_password(found, &form.password).await {
        // No second factor on the console.
        Ok(Some(op)) => op.complete(&state.registry).await,
        Ok(None) => {
            attempt.failed().await;
            send_user_login_failed(UserLoginFailedContext {
                source: "operator",
                attempted_username: Some(form.username.clone()),
                reason: AuthFailureReason::InvalidCredentials,
                request: meta,
            })
            .await;
            return Redirect::to(&format!(
                "/login?error=Invalid+credentials&next={}",
                urlencoding_lite(&next)
            ))
            .into_response();
        }
        Err(super::TenancyError::Busy) => return LoginRefused::Busy.into_response(),
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::operator_console", error = %e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };
    let oid = principal.id.get().copied().unwrap_or_default();
    attempt.succeeded().await;
    let mut payload = SessionPayload::new(
        oid,
        SESSION_TTL_SECS,
        session::PasswordFingerprint::of(&state.session_secret, &principal.password_hash),
    );
    payload.iat = crate::session::issued_at(principal.sessions_revoked_at);
    let cookie_value = session::encode(&state.session_secret, &payload);
    let cookie = Cookie::build((COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        // Audit H2 — Secure on the prod tier (HTTPS); off in dev so
        // local plain-HTTP login still works.
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(SESSION_TTL_SECS))
        .build();
    let mut resp = Redirect::to(&next).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("cookie is ascii"),
    );
    send_user_logged_in(UserLoggedInContext {
        source: "operator",
        user_id: oid,
        username: form.username.clone(),
        // Operator principals are uniformly "superuser-equivalent" at
        // the framework boundary — they're the people minting tenant
        // sessions. There's no separate flag on the Operator row.
        is_superuser: true,
        request: meta,
    })
    .await;
    resp
}

async fn logout(
    State(state): State<ConsoleState>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
) -> Response<Body> {
    use crate::signals::auth::{meta_from_parts, send_user_logged_out, UserLoggedOutContext};
    // Best-effort: decode the session cookie so the signal carries
    // operator_id. Receivers tolerate `None`.
    let oid = decode_operator_session(&headers, &state.session_secret);
    let meta = meta_from_parts(&extensions, &headers, Some("/logout"));
    // End the operator's sessions everywhere, impersonation ones included.
    let revoked = match live_operator_session(&state, &headers).await {
        Ok(Some((op, iat))) => crate::session::revoke_sessions::<auth::Operator>(
            &state.registry,
            op.id.get().copied().unwrap_or_default(),
            op.sessions_revoked_at,
            iat,
        )
        .await
        .map(drop),
        Ok(None) => Ok(()),
        // Never report a logout that did not happen.
        Err(e) => Err(e),
    };
    if let Err(e) = revoked {
        tracing::warn!(target: "rustango::tenancy::operator_console", error = %e, "logout revoke");
        return (StatusCode::INTERNAL_SERVER_ERROR, "logout failed").into_response();
    }
    let clear = Cookie::build((COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        // Match the Secure attribute used when the cookie was set so the
        // browser reliably clears it (audit H2).
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(0))
        .build();
    let mut resp = Redirect::to("/login").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear.to_string()).expect("cookie is ascii"),
    );
    send_user_logged_out(UserLoggedOutContext {
        source: "operator",
        user_id: oid,
        username: None,
        request: meta,
    })
    .await;
    resp
}

/// The operator and cookie `iat` behind a still-valid session cookie, so a
/// stale cookie cannot end the newer sessions. `Err` when the lookup fails.
async fn live_operator_session(
    state: &ConsoleState,
    headers: &axum::http::HeaderMap,
) -> Result<Option<(auth::Operator, i64)>, crate::sql::ExecError> {
    let Some(payload) = crate::cookies::cookie_from_headers(headers, COOKIE_NAME)
        .and_then(|val| session::decode(&state.session_secret, val).ok())
    else {
        return Ok(None);
    };
    let op = auth::Operator::objects()
        .where_(auth::Operator::id.eq(payload.oid))
        .fetch(&state.registry)
        .await?
        .into_iter()
        .next();
    Ok(op
        .filter(|op| {
            session::session_survives(
                &state.session_secret,
                &payload.pwf,
                payload.iat,
                &op.password_hash,
                op.password_changed_at,
                op.sessions_revoked_at,
            )
        })
        .map(|op| (op, payload.iat)))
}

/// Best-effort: decode the operator session cookie to recover the
/// operator id for audit signals. `None` on any error.
fn decode_operator_session(headers: &axum::http::HeaderMap, secret: &SessionSecret) -> Option<i64> {
    let val = crate::cookies::cookie_from_headers(headers, COOKIE_NAME)?;
    session::decode(secret, val).ok().map(|p| p.oid)
}

// ----------------------------- views

/// `GET /change-password` — render the operator self-serve
/// change-password form (#77, v0.29). Lives behind
/// `require_session` so unauthenticated requests bounce to login.
async fn change_password_form(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response<Body> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "change_password");
    ctx.insert("operator_username", &op.username);
    ctx.insert("error", &params.get("error"));
    ctx.insert("success", &params.get("ok"));
    render(&state, "op_change_password.html", &ctx)
}

#[derive(Debug, serde::Deserialize)]
struct OpChangePasswordForm {
    current_password: String,
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

/// `POST /change-password`: check the current password, hash the new
/// one, save it and bump `password_changed_at`.
///
/// `require_session` rejects cookies minted under the old password,
/// so this request is the last one the current cookie can serve; the
/// next click goes to login.
async fn change_password_submit(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    ip: crate::login_throttle::ClientIp,
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
    if let Err(e) = crate::password_validators::check_builtin_form_password(&form.new_password) {
        return redir_err(&e.message);
    }

    let op_id = op.id.get().copied().unwrap_or(0);
    if op_id <= 0 {
        return redir_err("Session is missing an operator id; please log in again.");
    }

    // `op` from the extension is a snapshot taken in `require_session`;
    // verify against the live hash.
    let op_row: auth::Operator = match auth::Operator::objects()
        .where_(auth::Operator::id.eq(op_id))
        .fetch(&state.registry)
        .await
    {
        Ok(rows) => match rows.into_iter().next() {
            Some(r) => r,
            None => {
                return redir_err("Your account no longer exists; please log in again.");
            }
        },
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::operator_console", error = %e, "change-password lookup");
            return (StatusCode::INTERNAL_SERVER_ERROR, "lookup failed").into_response();
        }
    };
    let verify = async {
        match super::password::verify_async(&form.current_password, &op_row.password_hash).await {
            Ok(ok) => Ok(ok),
            Err(super::TenancyError::Busy) => Err(crate::login_throttle::LoginRefused::Busy),
            Err(_) => Ok(false),
        }
    };
    let ok = match crate::login_throttle::shared()
        .verify_current_password(
            &crate::login_throttle::LoginScope::Operator,
            &ip,
            &op_row.username,
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
            return redir_err(&withheld_in(
                &tracing::error_span!("operator_console", operator = %op.username),
                "operator_console::change_password",
                "Could not hash the password",
                &e,
            ))
        }
    };
    match super::password::store_new_hash(&state.registry, &op_row, new_hash).await {
        Ok(true) => {}
        Ok(false) => return redir_err("Your password changed meanwhile; please try again."),
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::operator_console", error = %e, "change-password update");
            return (StatusCode::INTERNAL_SERVER_ERROR, "update failed").into_response();
        }
    }
    emit_registry_audit(
        &state.registry,
        "rustango_operators",
        &op_id.to_string(),
        op_id,
        "change_password",
        serde_json::Map::new(),
    )
    .await;
    redir("ok=Password+updated")
}

async fn welcome(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "home");
    ctx.insert("operator_username", &op.username);
    render(&state, "op_welcome.html", &ctx)
}

async fn orgs_list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<ListQuery>,
) -> Response<Body> {
    // Paged: this fetched every tenant, which is fine for the three a
    // demo has and not for the thousands a real registry holds.
    // Ordered so the page boundaries are stable between requests — an
    // unordered `LIMIT` may hand back the same row on two pages.
    use crate::core::Model as _;
    let paged = match Paged::of_model(&state.registry, super::Org::SCHEMA, q.page).await {
        Ok(p) => p,
        Err(e) => return server_error("operator_console", &e),
    };
    let rows: Vec<super::Org> = match super::Org::objects()
        .order_by(&[("slug", false)])
        .limit(paged.limit)
        .offset(paged.offset)
        .fetch(&state.registry)
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error("operator_console", &e),
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
    // Drives the "New tenant" button.
    ctx.insert("provisioning_enabled", &state.provisioner.is_some());
    ctx.insert("error", &q.error);
    ctx.insert("notice", &q.notice);
    paged.inject(&mut ctx, "/orgs", "");
    render(&state, "op_orgs.html", &ctx)
}

// ---- Shared SSO providers (registry-wide, `admin-sso`) --------------
// A provider defined here (`SharedSsoProvider`, registry scope) is
// offered on every tenant's login page. Each tenant manages its own
// providers (`SsoProvider`) from its own admin. Both lists merge at
// login; the tenant one wins on a slug clash.
#[cfg(feature = "admin-sso")]
#[derive(serde::Deserialize)]
struct SharedSsoForm {
    slug: String,
    label: String,
    kind: String,
    issuer_url: Option<String>,
    client_id: String,
    client_secret: String,
    scopes: Option<String>,
    sort_order: Option<i32>,
    enabled: Option<String>,
    allow_email_link: Option<String>,
}

#[cfg(feature = "admin-sso")]
async fn sso_shared_list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    let mut rows: Vec<super::sso::SharedSsoProvider> =
        match super::sso::SharedSsoProvider::objects()
            .fetch(&state.registry)
            .await
        {
            Ok(r) => r,
            Err(e) => return server_error("operator_console", &e),
        };
    rows.sort_by_key(|p| p.sort_order);
    let view: Vec<_> = rows
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id.get().copied().unwrap_or_default(),
                "slug": p.slug,
                "label": p.label,
                "kind": p.kind,
                "issuer_url": p.issuer_url,
                "client_id": p.client_id,
                "enabled": p.enabled,
                "allow_email_link": p.allow_email_link,
                "sort_order": p.sort_order,
            })
        })
        .collect();
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "sso");
    ctx.insert("operator_username", &op.username);
    ctx.insert("providers", &view);
    ctx.insert("edit_enabled", &state.pools.is_some());
    render(&state, "op_sso_shared.html", &ctx)
}

#[cfg(feature = "admin-sso")]
async fn sso_shared_create(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<SharedSsoForm>,
) -> Response<Body> {
    let mut row = super::sso::SharedSsoProvider {
        id: crate::sql::Auto::Unset,
        slug: form.slug.trim().to_owned(),
        label: form.label,
        kind: form.kind.trim().to_owned(),
        issuer_url: form.issuer_url.filter(|s| !s.trim().is_empty()),
        client_id: form.client_id,
        client_secret: crate::casts::Cast::new(form.client_secret),
        enabled: form.enabled.as_deref() == Some("on"),
        sort_order: form.sort_order.unwrap_or(0),
        scopes: form.scopes.filter(|s| !s.trim().is_empty()),
        allow_email_link: form.allow_email_link.as_deref() == Some("on"),
        created_at: crate::sql::Auto::Unset,
        updated_at: crate::sql::Auto::Unset,
    };
    if let Err(e) = row.insert_pool(&state.registry).await {
        return server_error("create failed", &e);
    }
    let mut detail = serde_json::Map::new();
    detail.insert("slug".into(), serde_json::json!(row.slug));
    detail.insert("kind".into(), serde_json::json!(row.kind));
    detail.insert("enabled".into(), serde_json::json!(row.enabled));
    detail.insert(
        "allow_email_link".into(),
        serde_json::json!(row.allow_email_link),
    );
    let id = row.id.get().copied().unwrap_or_default();
    sso_shared_audit(&state, &op, id, "sso_shared_create", detail).await;
    Redirect::to("/sso-shared").into_response()
}

/// Audit a shared provider change; these rows decide sign-in for every tenant.
#[cfg(feature = "admin-sso")]
async fn sso_shared_audit(
    state: &ConsoleState,
    op: &auth::Operator,
    id: i64,
    verb: &str,
    detail: serde_json::Map<String, serde_json::Value>,
) {
    use crate::core::Model as _;
    emit_registry_audit(
        &state.registry,
        super::sso::SharedSsoProvider::SCHEMA.table,
        &id.to_string(),
        op.id.get().copied().unwrap_or(0),
        verb,
        detail,
    )
    .await;
}

/// The wanted `allow_email_link` value, `on` or `off`.
#[cfg(feature = "admin-sso")]
#[derive(serde::Deserialize)]
struct EmailLinkForm {
    allow_email_link: String,
}

/// Set `allow_email_link` in place (only that column), so the id and its links survive.
#[cfg(feature = "admin-sso")]
async fn sso_shared_set_email_link(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Form(form): Form<EmailLinkForm>,
) -> Response<Body> {
    use crate::sql::UpdaterPool as _;
    let allow = match form.allow_email_link.as_str() {
        "on" => true,
        "off" => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "allow_email_link must be on or off",
            )
                .into_response()
        }
    };
    match super::sso::SharedSsoProvider::objects()
        .filter("id", id)
        .update()
        .set("allow_email_link", allow)
        .execute_pool(&state.registry)
        .await
    {
        Ok(1) => {
            let mut detail = serde_json::Map::new();
            detail.insert("allow_email_link".into(), serde_json::json!(allow));
            sso_shared_audit(&state, &op, id, "sso_shared_email_link", detail).await;
            Redirect::to("/sso-shared").into_response()
        }
        Ok(_) => (StatusCode::NOT_FOUND, "no such shared provider").into_response(),
        Err(e) => server_error("update failed", &e),
    }
}

#[cfg(feature = "admin-sso")]
async fn sso_shared_delete(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Response<Body> {
    let row = match super::sso::SharedSsoProvider::objects()
        .filter("id", id)
        .fetch(&state.registry)
        .await
    {
        Ok(v) => v.into_iter().next(),
        Err(e) => return server_error("delete failed", &e),
    };
    let Some(r) = row else {
        return (StatusCode::NOT_FOUND, "no such shared provider").into_response();
    };
    if let Err(e) = r.delete_pool(&state.registry).await {
        return server_error("delete failed", &e);
    }
    let mut detail = serde_json::Map::new();
    detail.insert("slug".into(), serde_json::json!(r.slug));
    sso_shared_audit(&state, &op, id, "sso_shared_delete", detail).await;
    Redirect::to("/sso-shared").into_response()
}

// ----------------------------- /orgs/{slug}/edit
//
// Built on the admin form pipeline:
// [`crate::admin::render::render_input`] for the `<input>` HTML,
// [`crate::admin::render::render_value_for_input`] for the prefill, and
// [`crate::forms::collect_values`] to parse the submit against
// `Org::SCHEMA` with per-field checks.
//
// Three things are specific to this form: the lock list below,
// `database_url` masking, and calling [`TenantPools::invalidate`] when
// `database_url` changes so the next request rebuilds the pool.

/// `Org` fields that are display-only on the edit form.
///
/// `logo_path` and `favicon_path` come from the upload sub-form
/// instead. `backend_kind` is locked because switching backends would
/// leave the tenant's data on the old driver; use the
/// `migrate-tenant-storage` verb for that.
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
        .fetch(&state.registry)
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error("operator_console", &e),
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
    // The `/__brand__/` fallback is a console route; a storage URL is not.
    let prefix = mount::MountPrefix::current();
    let console_url = |u: String| {
        if u.starts_with("/__brand__/") {
            prefix.url(&u)
        } else {
            u
        }
    };
    let logo_url = branding::brand_asset_url(&slug, logo_path.as_deref(), &state.brand_storage)
        .map(console_url);
    let favicon_url =
        branding::brand_asset_url(&slug, favicon_path.as_deref(), &state.brand_storage)
            .map(console_url);

    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("slug", &slug);
    // Drives the "Run migrations" button: migrations ride with the
    // provisioner, which owns the migrations directory.
    ctx.insert("provisioning_enabled", &state.provisioner.is_some());
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
    render(&state, "op_orgs_edit.html", &ctx)
}

/// `POST /orgs/{slug}/edit`: parse the form with
/// [`crate::forms::collect_values`] and write the columns it supplied
/// through [`crate::tenancy::org_edit::apply_values`].
///
/// Two side effects: changing `database_url` calls
/// [`TenantPools::invalidate`] so the next request rebuilds the pool,
/// and `active = false` makes the resolver return 404 for that tenant.
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

    // A blank `database_url` means "keep the current one". Drop it from
    // the form and add it to the skip list, or `collect_values` reads
    // the missing field as NULL and wipes the stored URL.
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
    // An unticked checkbox is absent from the form; `collect_values`
    // reads it as `false` for a NOT NULL bool (`active`), NULL if nullable.

    let collected = match crate::forms::collect_values(super::Org::SCHEMA, &form, &skip) {
        Ok(v) => v,
        Err(e) => return redirect_with_error(&slug, &e.to_string()),
    };
    if collected.is_empty() {
        return redirect_with_error(&slug, "no editable fields supplied");
    }

    // The CLI's write path, so the console runs the same routing
    // validators and host-clash check (#1931).
    let applied =
        match crate::tenancy::org_edit::apply_values(&state.registry, &slug, collected).await {
            Ok(a) => a,
            Err(e) => {
                let msg = e.user_facing().unwrap_or_else(|| {
                    withheld("operator_console::org_edit", &slug, "Could not save", &e)
                });
                return redirect_with_error(&slug, &msg);
            }
        };
    let database_url_changed = applied.database_url_rotated;

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
    let touched_cols: Vec<&str> = applied
        .touched
        .iter()
        .copied()
        .filter(|c| *c != DATABASE_URL_FIELD)
        .collect();
    detail.insert("fields".into(), serde_json::json!(touched_cols));
    if database_url_changed {
        detail.insert("database_url_rotated".into(), serde_json::json!(true));
    }
    emit_op_audit(&state.registry, &slug, operator_id, "edit", detail).await;

    let notice = if database_url_changed {
        format!(
            "updated `{slug}` (this server switched now; others within {} s)",
            crate::tenancy::resolver::CACHE_TTL.as_secs()
        )
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

/// Write one audit row for an operator action on `rustango_orgs`. The
/// changes blob always holds `tenant_slug` and `operator_id`; add more
/// keys through `extra`. A write failure is logged, never fatal.
///
/// `source` reads `operator:<id>:<verb>`, so a later search can tell
/// operator activity from tenant-user activity.
async fn emit_op_audit(
    registry: &crate::sql::Pool,
    slug: &str,
    operator_id: i64,
    verb: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) {
    let mut changes = extra;
    changes.insert(
        "tenant_slug".into(),
        serde_json::Value::String(slug.to_owned()),
    );
    emit_registry_audit(registry, "rustango_orgs", slug, operator_id, verb, changes).await;
}

/// The same trail for an action whose subject is not a tenant, so the
/// entity column names the real table. [`emit_op_audit`] is the
/// `rustango_orgs` shorthand over this.
async fn emit_registry_audit(
    registry: &crate::sql::Pool,
    entity_table: &'static str,
    entity_pk: &str,
    operator_id: i64,
    verb: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) {
    let mut changes = extra;
    changes.insert("operator_id".into(), serde_json::json!(operator_id));

    // Log the action here, not at each call site, so the log and the
    // audit trail always cover the same actions. Field *names* only:
    // logging the whole blob would leak a credential the day a caller
    // puts a value in it.
    let fields: Vec<&str> = changes
        .keys()
        .filter(|k| k.as_str() != "operator_id")
        .map(String::as_str)
        .collect();
    tracing::info!(
        target: ACTION_TARGET,
        event = "operator_action",
        action = verb,
        operator_id,
        entity = entity_table,
        entity_id = entity_pk,
        fields = fields.join(","),
        "operator action",
    );

    let entry = crate::audit::PendingEntry {
        entity_table,
        entity_pk: entity_pk.to_owned(),
        operation: crate::audit::AuditOp::Action,
        source: crate::audit::AuditSource::Custom(format!("operator:{operator_id}:{verb}")),
        changes: serde_json::Value::Object(changes),
    };
    if let Err(e) = crate::audit::emit_one_pool(registry, &entry).await {
        // The action happened but was not recorded. Loud on purpose: a
        // trail with a silent hole in it is worse than a short one.
        tracing::error!(
            target: ACTION_TARGET,
            event = "operator_action_unrecorded",
            action = verb,
            operator_id,
            entity = entity_table,
            entity_id = entity_pk,
            error = %e,
            "operator action was NOT written to the audit log",
        );
    }
}

// ----------------------------- /orgs/{slug}/edit/branding (multipart)
//
// The main edit form is url-encoded and posts the org's scalar config.
// Uploads need multipart, so they get their own sub-form.
// `branding::save_brand_asset` checks each part's content type and
// size. Once the file is stored we update `Org.{logo,favicon}_path`.
async fn org_edit_branding(
    State(state): State<ConsoleState>,
    axum::extract::Path(slug): axum::extract::Path<String>,
    Extension(op): Extension<auth::Operator>,
    mut mp: Multipart,
) -> Response<Body> {
    let mut updates: Vec<(BrandAssetKind, &'static str, String)> = Vec::new();
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
            Err(e) => {
                // Body limit or a cut-off upload: the client's doing, not a 500.
                tracing::warn!(
                    target: "rustango::error",
                    context = "operator_console::branding",
                    org = %slug,
                    error = %e,
                    "rejected upload"
                );
                return redirect_with_error(&slug, "upload too large or incomplete");
            }
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
                updates.push((kind, column, filename));
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
            Err(e) => {
                return redirect_with_error(
                    &slug,
                    &withheld("operator_console::branding", &slug, "Upload failed", &e),
                )
            }
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
        .map(|(_, col, v)| crate::core::Assignment {
            column: *col,
            value: crate::core::SqlValue::String(v.clone()).into(),
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
        return redirect_with_error(
            &slug,
            &withheld(
                "operator_console::branding",
                &slug,
                "Could not save the branding",
                &e,
            ),
        );
    }
    // Only now does no column name the old file (#1933).
    for (kind, _, kept) in &updates {
        if let Err(e) = branding::prune_brand_asset(&slug, *kind, kept, &state.brand_storage).await
        {
            tracing::warn!(slug = %slug, error = %e, "stale brand file not deleted");
        }
    }
    // Branding lives on the `Org` row that resolution caches, so an
    // upload without this leaves the previous logo rendering until the
    // entry expires.
    super::invalidate_org_cache();

    // Audit row — branding uploads touch the public-facing surface
    // of a tenant; operators iterating during onboarding leave a
    // breadcrumb trail. Records which assets landed (`logo`,
    // `favicon`) without the binary blob.
    let operator_id = op.id.get().copied().unwrap_or(0);
    let assets: Vec<String> = updates
        .iter()
        .map(|(_, col, _)| match *col {
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
        Err(e) => server_error("operator_console::brand_asset", &e),
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

// The crate's one query-value encoder (#1663); it also escapes `/`.
use crate::url_codec::url_encode as urlencoding_lite;

/// Check a caller-supplied `?next=` before it reaches `Location`.
///
/// Only same-origin relative paths pass, so a post-login redirect
/// cannot be pointed at another site. The check itself lives in
/// [`crate::auth_decorators::safe_next`].
fn sanitize_next(next: Option<&str>) -> String {
    next.and_then(crate::auth_decorators::safe_next)
        .unwrap_or_else(|| "/".to_owned())
}

// ============================================================== /orgs/{slug}/impersonate
//
// Behind the "Open admin as superuser" button on `/orgs/{slug}/edit`.
// The console mints a short-lived signed `HandoffPayload` and redirects
// to `<sub>.<apex><handoff_url>?token=<...>`. The tenant admin redeems
// the token and sets a host-scoped cookie, which browsers accept even
// on `localhost`.
//
// A banner and audit rows on both ends keep impersonation visible.

async fn org_impersonate(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    headers: HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response<Body> {
    let Some(tenant_secret) = state.tenant_session_secret.clone() else {
        // Unreachable: the route is mounted only with a secret.
        return (StatusCode::SERVICE_UNAVAILABLE, "impersonation disabled").into_response();
    };
    // Look up the org so we can refuse an inactive tenant and give the
    // audit row the right context.
    let orgs: Vec<super::Org> = match super::Org::objects()
        .where_(super::Org::slug.eq(slug.clone()))
        .fetch(&state.registry)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::operator_console", error = %e, "org lookup failed");
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
    let mut payload = handoff::HandoffPayload::new(
        operator_id,
        slug.clone(),
        handoff::HANDOFF_TTL_SECS,
        handoff::PasswordFingerprint::of(&tenant_secret, &op.password_hash),
    );
    payload.iat = crate::session::issued_at(op.sessions_revoked_at);
    let token = handoff::mint(&tenant_secret, &payload);

    // Audit row on the operator side. The tenant admin writes its own
    // when an impersonated session first makes a change.
    let mut detail = serde_json::Map::new();
    detail.insert(
        "action".into(),
        serde_json::Value::String("impersonate.start".into()),
    );
    emit_op_audit(&state.registry, &slug, operator_id, "impersonating", detail).await;

    // Build the redirect: tenant subdomain, the handoff path, and the
    // token. See `tenant_origin` for the scheme.
    let prefix = handoff_prefix(org.path_prefix.as_deref());
    let host = if let Some(pat) = org.host_pattern.as_deref().filter(|s| !s.is_empty()) {
        pat.to_owned()
    } else {
        // No host pattern: a path-prefix tenant lives on the apex,
        // any other on `<slug>.<apex>`.
        let apex = crate::tenancy::server::apex_domain();
        if prefix.is_empty() {
            format!("{}.{}", slug, apex)
        } else {
            apex
        }
    };
    let port_suffix = handoff_port_suffix(
        org.port,
        std::env::var("RUSTANGO_TENANT_PORT").ok(),
        headers.get(header::HOST).and_then(|v| v.to_str().ok()),
    );
    let handoff_path = format!("{prefix}{}", state.tenant_handoff_url.trim_end_matches('/'));
    // The token is base64url (`URL_SAFE_NO_PAD`) + a single `.` —
    // every character is already URL-safe, so no escaping needed.
    let handoff = format!(
        "{}{handoff_path}",
        crate::tenancy::server::tenant_origin(&host, &port_suffix)
    );
    let redirect_to = format!("{handoff}?token={token}");

    let mut resp = Redirect::to(&redirect_to).into_response();
    // The token in the URL is single-use + short-lived, but
    // `Referrer-Policy: no-referrer` keeps it from leaking to
    // any third-party resource the destination page loads.
    resp.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    tracing::info!(
        target: "rustango::tenancy::operator_console",
        slug = %slug,
        operator_id,
        ttl_secs = handoff::HANDOFF_TTL_SECS,
        // Not the token: it is a live login until redeemed (#2107).
        handoff = %handoff,
        "minted impersonation handoff token",
    );
    resp
}

/// The org's path prefix for the handoff URL, or `""` when the stored
/// value is not one segment: `//evil.example` would redirect off-site.
fn handoff_prefix(path_prefix: Option<&str>) -> &str {
    path_prefix
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| crate::tenancy::provision::validate_path_prefix(p).is_ok())
        .unwrap_or("")
}

/// Port for the handoff URL. A port-routed org's own port wins (#1933);
/// then `RUSTANGO_TENANT_PORT`; then the console request's port.
fn handoff_port_suffix(
    org_port: Option<i32>,
    env_port: Option<String>,
    host: Option<&str>,
) -> String {
    let usable = |p: &str| !p.is_empty() && p != "80" && p != "443";
    if let Some(p) = org_port {
        let p = p.to_string();
        return if usable(&p) {
            format!(":{p}")
        } else {
            String::new()
        };
    }
    env_port
        .filter(|p| usable(p))
        .or_else(|| {
            host.and_then(|h| crate::urls::split_host_port(h)?.1)
                .map(|p| p.to_string())
                .filter(|p| usable(p))
        })
        .map(|p| format!(":{p}"))
        .unwrap_or_default()
}

/// Names only where the config broke; the error text can quote a secret.
#[cfg(feature = "config")]
fn warn_config_error(e: &crate::config::ConfigError) {
    tracing::warn!(
        target: "rustango::tenancy::operator_console",
        at = %e.location(),
        "the config does not load; console branding uses defaults"
    );
}

#[cfg(all(test, feature = "config", feature = "runtime"))]
mod config_warn_tests {
    #[test]
    fn a_broken_config_logs_its_line_not_its_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config/default.toml"),
            "[database]\nurl = \"postgres://app:s3cret@db/app\n",
        )
        .unwrap();
        let err = crate::config::Settings::load_from(&dir.path().join("config"), "dev")
            .expect_err("unterminated string");
        let out = crate::testkit::CaptureWriter::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || super::warn_config_error(&err));
        let logged = out.contents();
        assert!(!logged.contains("s3cret"), "{logged}");
        assert!(logged.contains("line 2"), "{logged}");
    }
}

#[cfg(all(test, feature = "runtime"))]
mod withheld_tests {
    #[test]
    fn the_withheld_log_line_names_the_org() {
        let out = crate::testkit::CaptureWriter::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || {
            super::withheld("ctx", "acme-org", "Upload failed", &"disk full")
        });
        let logged = out.contents();
        assert!(logged.contains("disk full"), "{logged}");
        assert!(logged.contains("org=acme-org"), "{logged}");
    }
}

#[cfg(test)]
mod handoff_port_tests {
    use super::{handoff_port_suffix, handoff_prefix};

    #[test]
    fn only_a_valid_path_prefix_reaches_the_handoff_url() {
        assert_eq!(handoff_prefix(Some("/acme/")), "/acme");
        assert_eq!(handoff_prefix(None), "");
        for bad in ["//evil.example", "/a/b", "acme", "/a?b", "/@evil"] {
            assert_eq!(handoff_prefix(Some(bad)), "", "`{bad}`");
        }
    }

    #[test]
    fn a_port_routed_org_lands_on_its_own_port() {
        let env = Some("9000".to_owned());
        assert_eq!(
            handoff_port_suffix(Some(8443), env.clone(), Some("ops:8080")),
            ":8443"
        );
        assert_eq!(handoff_port_suffix(None, env, Some("ops:8080")), ":9000");
        assert_eq!(handoff_port_suffix(None, None, Some("ops:8080")), ":8080");
        assert_eq!(handoff_port_suffix(Some(443), None, Some("ops:8080")), "");
        // A non-digit "port" cannot smuggle another host into the URL.
        for bad in ["ops:@evil.com", "[::1]:@evil.com", "ops:1@evil.com:2"] {
            assert_eq!(handoff_port_suffix(None, None, Some(bad)), "", "{bad}");
        }
        // An IPv6 host's colons are not a port (#2043).
        assert_eq!(handoff_port_suffix(None, None, Some("[::1]")), "");
        assert_eq!(handoff_port_suffix(None, None, Some("[::1]:8080")), ":8080");
    }
}

/// #1526. These exercise `sanitize_next` — the function the login
/// handler actually calls, whose result reaches `Redirect::to` — and
/// not the public `urls::url_has_allowed_host_and_scheme` helper,
/// which this codebase never calls and which the original fix went
/// into.
#[cfg(test)]
mod sanitize_next_tests {
    use super::sanitize_next;

    #[test]
    fn a_backslash_the_browser_rewrites_is_refused() {
        // Each reaches the network as protocol-relative `//evil…`
        // after the browser's `\` → `/` rewrite (WHATWG URL 4.4),
        // while starting with `/` in the source text.
        for hostile in [
            "/\\evil.example/x",
            "/\\\\evil.example/x",
            "\\/evil.example/x",
            "/%5Cevil.example/x",
        ] {
            assert_eq!(
                sanitize_next(Some(hostile)),
                "/",
                "`{hostile}` must not reach Location",
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
            assert_eq!(sanitize_next(Some(hostile)), "/", "{hostile}");
        }
        assert_eq!(sanitize_next(None), "/");
    }

    #[test]
    fn an_ordinary_path_still_survives() {
        // The control: tightening must not send every operator to `/`
        // after login, which would pass the assertions above.
        for ok in ["/orgs", "/orgs/acme/edit", "/orgs?page=2&q=a"] {
            assert_eq!(sanitize_next(Some(ok)), ok, "{ok} should survive");
        }
    }
}

#[cfg(test)]
mod redirect_to_login_tests {
    use super::redirect_to_login;

    /// `next` is fully encoded and decodes back to the path it came from.
    #[test]
    fn next_round_trips_through_the_location() {
        let resp = redirect_to_login("/orgs/acme edit?tab=1&x=2");
        let loc = resp.headers()[axum::http::header::LOCATION]
            .to_str()
            .unwrap();
        assert_eq!(loc, "/login?next=%2Forgs%2Facme%20edit%3Ftab%3D1%26x%3D2");
        let next = loc.strip_prefix("/login?next=").unwrap();
        assert_eq!(
            crate::url_codec::url_decode(next),
            "/orgs/acme edit?tab=1&x=2"
        );
    }
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
