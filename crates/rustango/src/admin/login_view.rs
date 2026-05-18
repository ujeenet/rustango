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

/// Public (unauthenticated) routes — `/login` + `/logout`. Merged
/// into the admin router BEFORE the auth middleware is applied so
/// the login form itself stays publicly reachable.
pub(crate) fn public_router(state: AppState) -> Router {
    Router::new()
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout_submit))
        .with_state(state)
}

/// Authenticated routes that ride on top of the session middleware.
/// `/account/password` lives here so an unauthenticated visitor
/// can't reach the password-change form. Mounted from
/// [`crate::admin::Builder::build`] when `with_session_auth` is set.
pub(crate) fn protected_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/account/password",
            get(change_password_form).post(change_password_submit),
        )
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
            username: form.username.clone(),
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

// ============================================================ Change password (GET + POST)

async fn change_password_form(State(state): State<AppState>) -> Html<String> {
    Html(render_change_password_form(&state, None, None))
}

#[derive(serde::Deserialize)]
struct ChangePasswordInput {
    current_password: String,
    new_password: String,
    new_password_confirm: String,
}

async fn change_password_submit(
    State(state): State<AppState>,
    Form(form): Form<ChangePasswordInput>,
) -> Response {
    // The middleware guarantees a session is in scope here; if not,
    // bail loudly — a request reaching this handler without one is
    // a programmer bug.
    let Some(session) = super::session::current() else {
        return (StatusCode::UNAUTHORIZED, "session required").into_response();
    };

    if form.new_password != form.new_password_confirm {
        return Html(render_change_password_form(
            &state,
            None,
            Some("Confirmation password did not match."),
        ))
        .into_response();
    }
    if form.new_password.len() < 8 {
        return Html(render_change_password_form(
            &state,
            None,
            Some("New password must be at least 8 characters."),
        ))
        .into_response();
    }

    // Look up the current row by user_id (from the session) so we
    // can verify the *current* password before mutating the hash.
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    let select = SelectQuery {
        model: AdminUser::SCHEMA,
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(session.user_id),
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
        return (StatusCode::UNAUTHORIZED, "user not found").into_response();
    };
    let stored_hash = row
        .get("password_hash")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !crate::passwords::verify(&form.current_password, stored_hash).unwrap_or(false) {
        return Html(render_change_password_form(
            &state,
            None,
            Some("Current password is incorrect."),
        ))
        .into_response();
    }

    let new_hash = match crate::passwords::hash(&form.new_password) {
        Ok(h) => h,
        Err(_) => {
            return Html(render_change_password_form(
                &state,
                None,
                Some("Internal hashing error."),
            ))
            .into_response();
        }
    };

    // Schema-driven UPDATE — keeps the bare admin compiling without
    // tenancy's typed query helpers.
    use crate::core::{Assignment, Expr, UpdateQuery};
    let q = UpdateQuery {
        model: AdminUser::SCHEMA,
        set: vec![Assignment {
            column: "password_hash",
            value: Expr::Literal(SqlValue::String(new_hash)),
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(session.user_id),
        }),
    };
    if let Err(e) = crate::sql::update_pool(&state.pool, &q).await {
        return Html(render_change_password_form(
            &state,
            None,
            Some(&format!("Update failed: {e}")),
        ))
        .into_response();
    }

    Html(render_change_password_form(
        &state,
        Some("Password updated."),
        None,
    ))
    .into_response()
}

fn render_change_password_form(
    state: &AppState,
    success: Option<&str>,
    error: Option<&str>,
) -> String {
    let admin_prefix = &state.config.admin_prefix;
    let mut ctx = serde_json::json!({
        "title": "Change password",
        "action": format!("{admin_prefix}/account/password"),
        "success": success,
        "error": error,
    });
    super::templates::render_with_chrome(
        "change_password.html",
        &mut ctx,
        super::helpers::chrome_context(state, None),
    )
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
    /// #253 slice C — when `true`, non-superuser sessions are
    /// rejected with a 403 page. Default for the bare admin; mirrors
    /// Django's `is_staff` requirement (the bare admin has no
    /// per-model permission system yet, so the only access tier is
    /// "superuser"). Future epics layering in real permissions can
    /// flip this off and consult a `user_perms` set instead.
    pub(crate) require_superuser: bool,
}

/// Gate every admin request behind a valid session cookie. The
/// `/login` route bypasses the gate (mounted before this middleware
/// applies on the outer Router); embedded static assets at
/// `/__static__/...` also pass through.
///
/// On valid session: inserts `Extension<AdminSession>` into the
/// request so handlers can read the current user. When
/// `gate.require_superuser` is set (the bare-admin default), a
/// non-superuser session is rejected with a 403 page — Django's
/// "must be staff to access /admin" shape.
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
        if gate.require_superuser && !session.is_superuser {
            // #253 slice C — render a 403 inline rather than redirect
            // to /login, so the operator gets a clear "you are signed
            // in but not allowed here" signal instead of an infinite
            // login → 403 → login loop.
            return forbidden_page(&session);
        }
        request.extensions_mut().insert(session.clone());
        // Scope the task-local so `chrome_context` (deep in the
        // template render stack) can read it without every handler
        // threading the session through its argument list.
        return super::session::CURRENT_SESSION
            .scope(session, next.run(request))
            .await;
    }

    // No valid session — bounce to the login form. Use 303 See Other
    // so the GET semantics are preserved (browsers follow with GET).
    Redirect::to(&gate.login_path).into_response()
}

/// #253 slice C — minimal 403 page for non-superuser sessions. Plain
/// HTML, no chrome (chrome rendering needs the same auth gate to
/// have already passed). The body invites the operator to contact
/// their administrator and offers a link to sign out + back to
/// login.
fn forbidden_page(session: &AdminSession) -> Response {
    // Tiny inline escape — the page renders BEFORE the admin chrome
    // (the gate fires before `next.run`), so we can't reach the
    // chrome's `render::escape` helper without rebuilding state.
    let mut username = String::with_capacity(session.username.len());
    for ch in session.username.chars() {
        match ch {
            '&' => username.push_str("&amp;"),
            '<' => username.push_str("&lt;"),
            '>' => username.push_str("&gt;"),
            '"' => username.push_str("&quot;"),
            '\'' => username.push_str("&#39;"),
            other => username.push(other),
        }
    }
    let body = format!(
        "<!doctype html>\
         <html><head><title>Forbidden</title>\
         <style>body{{font-family:system-ui;max-width:42em;margin:4em auto;padding:0 1em;line-height:1.5}}\
         h1{{font-size:1.4em}}\
         .meta{{color:#666;font-size:.9em}}\
         </style></head><body>\
         <h1>403 — Admin access required</h1>\
         <p>You are signed in as <strong>{username}</strong>, but only \
         superusers can use the admin.</p>\
         <p class=\"meta\">Ask your administrator to grant superuser \
         status, or sign out below if this isn't the account you \
         intended to use.</p>\
         <form method=\"post\" action=\"/logout\">\
           <button type=\"submit\">Sign out</button>\
         </form>\
         </body></html>"
    );
    let mut resp = Html(body).into_response();
    *resp.status_mut() = StatusCode::FORBIDDEN;
    resp
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
