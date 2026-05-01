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
//! * `GET  /__static__/rustango.png` — embedded asset
//!
//! Read-only on purpose for v1 — mutations on operators / orgs run
//! through the `manage` CLI so the side-effects (CREATE SCHEMA,
//! migrations) happen atomically.
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

use axum::body::Body;
use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Extension, Router};
use cookie::time::Duration as CookieDuration;
use cookie::{Cookie, SameSite};
use crate::core::Column as _;
use crate::sql::sqlx::PgPool;
use crate::sql::Fetcher;
use serde::Deserialize;
use std::sync::Arc;
use tera::{Context, Tera};

pub use session::{SessionPayload, SessionSecret, SessionSecretError};
use session::{COOKIE_NAME, SESSION_TTL_SECS};

use super::auth;

const RUSTANGO_PNG: &[u8] = include_bytes!("../static/rustango.png");

#[derive(Clone)]
struct ConsoleState {
    registry: PgPool,
    session_secret: Arc<SessionSecret>,
    tera: Arc<Tera>,
}

/// Build the operator-console `axum::Router`. Mount at the apex
/// (production: `app.example.com`; demo: `localhost:8080`) — the
/// console expects to live at the root, not nested under a path.
#[must_use]
pub fn router(registry: PgPool, secret: SessionSecret) -> Router {
    let mut tera = Tera::default();
    tera.add_raw_templates([
        ("op_layout.html", include_str!("../templates/op_layout.html")),
        ("op_login.html", include_str!("../templates/op_login.html")),
        ("op_welcome.html", include_str!("../templates/op_welcome.html")),
        ("op_operators.html", include_str!("../templates/op_operators.html")),
        ("op_orgs.html", include_str!("../templates/op_orgs.html")),
    ])
    .expect("operator-console templates parse");
    let state = ConsoleState {
        registry,
        session_secret: Arc::new(secret),
        tera: Arc::new(tera),
    };

    // Public routes (login + static asset) skip the auth gate.
    let public = Router::new()
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout))
        .route("/__static__/rustango.png", get(static_rustango_png));

    // Authenticated routes: the middleware injects an `Extension<auth::Operator>`.
    let private = Router::new()
        .route("/", get(welcome))
        .route("/operators", get(operators_list))
        .route("/orgs", get(orgs_list))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    public.merge(private).with_state(state)
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
    let principal = match auth::authenticate_operator(
        &state.registry,
        &form.username,
        &form.password,
    )
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

async fn welcome(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Html<String> {
    let mut ctx = Context::new();
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
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("orgs", &view);
    Html(state.tera.render("op_orgs.html", &ctx).unwrap_or_default()).into_response()
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
