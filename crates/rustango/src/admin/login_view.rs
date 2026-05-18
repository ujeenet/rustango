//! `GET /login` + `POST /login` + `POST /logout` + auth middleware
//! for the bare admin's session auth (#253 slice A).
//!
//! Mounted by [`crate::admin::Builder::with_session_auth`]. Layered
//! as middleware so every non-login route requires a valid session
//! cookie; the gate redirects to `/login` (relative to
//! `state.config.admin_prefix`) on missing / expired cookies.
//!
//! ## Reuse with `tenancy::admin`
//!
//! The HMAC signing primitive comes from `crate::session` (shared
//! with `tenancy::session`). The password-verify call goes through
//! `crate::passwords::verify` — the same primitive the tenancy
//! `auth::authenticate_user` flow uses. Only the user model
//! (`AdminUser` vs `tenancy::User`) and the cookie shape differ;
//! all the crypto + password machinery lives in one place.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;

use super::session::{self, AdminSession, AdminSessionSecret, SESSION_COOKIE};
use super::templates::render_template;
use super::urls::AppState;
use super::user::AdminUser;
use crate::core::{Filter, Model, Op, SelectQuery, SqlValue, WhereExpr};

/// Mount the login + logout routes. Returns a `Router` that should
/// be merged into the admin router BEFORE the auth middleware is
/// applied so the login form itself stays publicly reachable.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout_submit))
        .with_state(state)
}

// ============================================================ Login form (GET)

async fn login_form(State(state): State<AppState>) -> Html<String> {
    Html(render_login_form(&state, None))
}

fn render_login_form(state: &AppState, error: Option<&str>) -> String {
    let admin_prefix = &state.config.admin_prefix;
    let ctx = serde_json::json!({
        "title": "Sign in",
        "action": format!("{admin_prefix}/login"),
        "error": error,
        "admin_title": state
            .config
            .title
            .as_deref()
            .unwrap_or("Rustango Admin"),
        "admin_prefix": admin_prefix,
        "static_url": &state.config.static_url,
    });
    render_template("login.html", &ctx)
}

// ============================================================ Login form (POST)

#[derive(serde::Deserialize)]
struct LoginInput {
    username: String,
    password: String,
}

async fn login_submit(State(state): State<AppState>, Form(form): Form<LoginInput>) -> Response {
    let Some(secret) = state.config.session_secret.clone() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "session auth not configured",
        )
            .into_response();
    };

    // Schema-driven lookup so we don't depend on tenancy's
    // typed query helpers — the bare admin compiles without `tenancy`.
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    let select = SelectQuery {
        model: AdminUser::SCHEMA,
        where_clause: WhereExpr::Predicate(Filter {
            column: "username",
            op: Op::Eq,
            value: SqlValue::String(form.username.clone()),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
        lock_mode: None,
        compound: vec![],
        projection: None,
    };
    let row = crate::sql::select_one_row_as_json(&state.pool, &select, &fields)
        .await
        .ok()
        .flatten();

    let Some(row) = row else {
        return Html(render_login_form(&state, Some("Invalid credentials."))).into_response();
    };
    let id = row.get("id").and_then(|v| v.as_i64()).unwrap_or_default();
    let stored_hash = row
        .get("password_hash")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let is_active = row.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
    let is_superuser = row
        .get("is_superuser")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !is_active {
        return Html(render_login_form(&state, Some("Account is disabled."))).into_response();
    }
    if !crate::passwords::verify(&form.password, stored_hash).unwrap_or(false) {
        return Html(render_login_form(&state, Some("Invalid credentials."))).into_response();
    }

    let cookie_value = session::encode(
        &secret,
        AdminSession {
            user_id: id,
            is_superuser,
        },
    );
    let cookie = format!(
        "{name}={val}; Path=/; HttpOnly; SameSite=Lax",
        name = SESSION_COOKIE,
        val = cookie_value,
    );
    let redirect_to = if state.config.admin_prefix.is_empty() {
        "/".to_owned()
    } else {
        state.config.admin_prefix.clone()
    };
    let mut resp = Redirect::to(&redirect_to).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

// ============================================================ Logout (POST)

async fn logout_submit(State(state): State<AppState>) -> Response {
    let cookie = format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        name = SESSION_COOKIE,
    );
    let mut resp = Redirect::to(&format!("{}/login", state.config.admin_prefix)).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

// ============================================================ Middleware

/// State threaded into the auth middleware — the signing secret +
/// the login URL to redirect to on missing session. Cloned per
/// request, kept Arc<…> so the underlying key isn't copied.
#[derive(Clone)]
pub(crate) struct SessionGate {
    pub(crate) secret: Arc<AdminSessionSecret>,
    pub(crate) login_path: String,
}

/// Gate every admin request behind a valid session cookie. The
/// `/login` route bypasses the gate (mounted before this middleware
/// applies on the outer Router); embedded static assets at
/// `/__static__/...` also pass through.
///
/// On valid session: inserts `Extension<AdminSession>` into the
/// request so handlers can read the current user.
pub(crate) async fn require_session(
    State(gate): State<SessionGate>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path == gate.login_path || path == "/login" || path.starts_with("/__static__") {
        return next.run(request).await;
    }

    if let Some(session) = read_session_cookie(&request, &gate.secret) {
        request.extensions_mut().insert(session);
        return next.run(request).await;
    }

    // No valid session — bounce to the login form. Use 303 See Other
    // so the GET semantics are preserved (browsers follow with GET).
    Redirect::to(&gate.login_path).into_response()
}

fn read_session_cookie(req: &Request<Body>, secret: &AdminSessionSecret) -> Option<AdminSession> {
    let raw = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';').map(str::trim) {
        if let Some(val) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return session::decode(secret, val);
        }
    }
    None
}
