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

async fn login_form(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    login_response(&state, &headers, None)
}

/// Build the login-page response, seeding a double-submit CSRF token
/// (audit M3): the cookie is set on the GET (if not already present) and
/// the matching token is embedded as a hidden form field, so the POST
/// can be validated in [`login_submit`] without relying on outer
/// middleware placement.
fn login_response(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    error: Option<&str>,
) -> Response {
    use crate::forms::csrf;
    let (token, set_cookie) = csrf::ensure_token(headers, csrf::CSRF_COOKIE);
    let html = render_login_form(state, error, &csrf::csrf_input_html(&token));
    let mut resp = Html(html).into_response();
    if let Some(cookie) = set_cookie {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

fn render_login_form(state: &AppState, error: Option<&str>, csrf_input: &str) -> String {
    let admin_prefix = &state.config.admin_prefix;
    let ctx = serde_json::json!({
        "title": "Sign in",
        "action": format!("{admin_prefix}/login"),
        "error": error,
        "csrf_input": csrf_input,
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
    /// Double-submit CSRF token (audit M3). Optional so a missing field
    /// is handled as a failed check (re-render) rather than a 422 form
    /// rejection.
    #[serde(rename = "_csrf", default)]
    csrf_token: Option<String>,
}

async fn login_submit(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginInput>,
) -> Response {
    use crate::signals::auth::{
        meta_from_headers, send_user_logged_in, send_user_login_failed, AuthFailureReason,
        UserLoggedInContext, UserLoginFailedContext,
    };
    let meta = meta_from_headers(&headers, Some("/login"));

    let Some(secret) = state.config.session_secret.clone() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "session auth not configured",
        )
            .into_response();
    };

    // Audit M3 — validate the double-submit CSRF token before touching
    // the database or verifying credentials. A cross-site forged POST
    // can't read the SameSite=Lax CSRF cookie to echo it back, so it
    // fails here. The token is seeded + embedded by `login_response`.
    if !crate::forms::csrf::verify_form_token(&headers, form.csrf_token.as_deref()) {
        return login_response(
            &state,
            &headers,
            Some("Your session expired or the form was invalid. Please try again."),
        );
    }

    // Schema-driven lookup so we don't depend on tenancy's
    // typed query helpers — the bare admin compiles without `tenancy`.
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    // #562 — by_pk constructor for the single-column-lookup shape.
    let select = SelectQuery::by_pk(
        AdminUser::SCHEMA,
        "username",
        SqlValue::String(form.username.clone()),
    );
    let row = crate::sql::select_one_row_as_json(&state.pool, &select, &fields)
        .await
        .ok()
        .flatten();

    let Some(row) = row else {
        // H1: spend a verify's worth of work on the unknown-user path
        // so timing doesn't reveal whether the username exists.
        crate::passwords::verify_dummy(&form.password);
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::InvalidCredentials,
            request: meta.clone(),
        })
        .await;
        return login_response(&state, &headers, Some("Invalid credentials."));
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

    // Audit M1 — per-account brute-force lockout, on by default. The key
    // is scoped (`admin:<id>`) so it can't collide with operator/tenant
    // ids, and uses the resolved id (not the raw username) so an attacker
    // can't lock arbitrary accounts. A locked account short-circuits
    // before the password verify.
    #[cfg(feature = "cache")]
    if crate::account_lockout::shared()
        .is_locked(&format!("admin:{id}"))
        .await
    {
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::InvalidCredentials,
            request: meta.clone(),
        })
        .await;
        return login_response(
            &state,
            &headers,
            Some("Too many failed attempts. Please try again later."),
        );
    }

    // Verify before the active check so active vs inactive accounts take
    // the same time (audit H1).
    let password_ok = crate::passwords::verify(&form.password, stored_hash).unwrap_or(false);

    if !is_active {
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::Inactive,
            request: meta.clone(),
        })
        .await;
        // Audit M4 — do NOT reveal that the account exists-but-disabled.
        // Return the same generic message as unknown-user / wrong-password
        // so the login form can't be used to enumerate accounts. The
        // Inactive signal above still records the real reason for audit.
        return login_response(&state, &headers, Some("Invalid credentials."));
    }
    if !password_ok {
        // Audit M1 — count this failure toward the per-account lockout.
        #[cfg(feature = "cache")]
        {
            let _ = crate::account_lockout::shared()
                .record_failure(&format!("admin:{id}"))
                .await;
        }
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::InvalidCredentials,
            request: meta.clone(),
        })
        .await;
        return login_response(&state, &headers, Some("Invalid credentials."));
    }

    // Audit M1 — successful login clears the failure counter + any lock.
    #[cfg(feature = "cache")]
    crate::account_lockout::shared()
        .clear(&format!("admin:{id}"))
        .await;

    // Audit N8 — bind the cookie to a fingerprint of the current
    // password hash so a password change/reset invalidates it.
    let auth_hash = session::password_fingerprint(&secret, stored_hash);
    let cookie_value = session::encode(
        &secret,
        AdminSession {
            user_id: id,
            username: form.username.clone(),
            is_superuser,
        },
        &auth_hash,
    );
    let cookie = format!(
        "{name}={val}; Path=/; HttpOnly; SameSite=Lax{secure}",
        name = SESSION_COOKIE,
        val = cookie_value,
        secure = if state.config.secure_cookies {
            "; Secure"
        } else {
            ""
        },
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
    send_user_logged_in(UserLoggedInContext {
        source: "admin",
        user_id: id,
        username: form.username.clone(),
        is_superuser,
        request: meta,
    })
    .await;
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
    // #562 — by_pk constructor.
    let select = SelectQuery::by_pk(AdminUser::SCHEMA, "id", SqlValue::I64(session.user_id));
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
            value: Expr::Literal(SqlValue::String(new_hash.clone())),
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

    // Audit N8 — the cookie this request carries holds the OLD password
    // fingerprint, so the gate would sign this session out on the next
    // request. Re-issue the cookie with the NEW fingerprint so the
    // current device stays signed in while every *other* device's
    // pre-change cookie is invalidated (mirrors Django's
    // update_session_auth_hash).
    let mut resp = Html(render_change_password_form(
        &state,
        Some("Password updated."),
        None,
    ))
    .into_response();
    if let Some(secret) = state.config.session_secret.as_ref() {
        let auth_hash = session::password_fingerprint(secret, &new_hash);
        let cookie_value = session::encode(
            secret,
            AdminSession {
                user_id: session.user_id,
                username: session.username.clone(),
                is_superuser: session.is_superuser,
            },
            &auth_hash,
        );
        let cookie = format!(
            "{name}={val}; Path=/; HttpOnly; SameSite=Lax{secure}",
            name = SESSION_COOKIE,
            val = cookie_value,
            secure = if state.config.secure_cookies {
                "; Secure"
            } else {
                ""
            },
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().insert(header::SET_COOKIE, v);
        }
    }
    resp
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

async fn logout_submit(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    use crate::signals::auth::{meta_from_headers, send_user_logged_out, UserLoggedOutContext};
    let meta = meta_from_headers(&headers, Some("/logout"));

    // Best-effort session decode so the signal carries the user id /
    // username when the cookie is still valid. Receivers that key off
    // those fields fall through to the `None` branch cleanly.
    let (user_id, username) = state
        .config
        .session_secret
        .as_ref()
        .and_then(|secret| {
            let raw = headers.get(header::COOKIE)?.to_str().ok()?;
            for part in raw.split(';').map(str::trim) {
                if let Some(val) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
                    if let Some(sess) = session::decode(secret, val) {
                        return Some((Some(sess.user_id), Some(sess.username)));
                    }
                }
            }
            None
        })
        .unwrap_or((None, None));

    let cookie = format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}",
        name = SESSION_COOKIE,
        secure = if state.config.secure_cookies {
            "; Secure"
        } else {
            ""
        },
    );
    let mut resp = Redirect::to(&format!("{}/login", state.config.admin_prefix)).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    send_user_logged_out(UserLoggedOutContext {
        source: "admin",
        user_id,
        username,
        request: meta,
    })
    .await;
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
    /// Audit N8 — pool for the per-request password-fingerprint check
    /// that invalidates cookies minted before a password change.
    pub(crate) pool: crate::sql::Pool,
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

    if let Some((session, cookie_auth_hash)) = read_session_cookie(&request, &gate.secret) {
        if gate.require_superuser && !session.is_superuser {
            // #253 slice C — render a 403 inline rather than redirect
            // to /login, so the operator gets a clear "you are signed
            // in but not allowed here" signal instead of an infinite
            // login → 403 → login loop.
            return forbidden_page(&session);
        }
        // Audit N8 — invalidate cookies minted before a password change
        // by comparing the cookie's password fingerprint against the
        // user's current hash. Fails closed for a deleted user / changed
        // password; tolerates a transient DB error (the cookie HMAC + exp
        // still bound it) so a hiccup doesn't sign everyone out.
        if !auth_hash_still_valid(&gate, session.user_id, &cookie_auth_hash).await {
            return Redirect::to(&gate.login_path).into_response();
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

fn read_session_cookie(
    req: &Request<Body>,
    secret: &AdminSessionSecret,
) -> Option<(AdminSession, String)> {
    let raw = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';').map(str::trim) {
        if let Some(val) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return session::decode_full(secret, val);
        }
    }
    None
}

/// Audit N8 — is the cookie's password fingerprint still current? Looks
/// up the user's live `password_hash` and recomputes the fingerprint.
/// `false` (force re-login) when the hash changed or the user is gone;
/// `true` on a transient DB error so a hiccup doesn't evict everyone.
async fn auth_hash_still_valid(gate: &SessionGate, user_id: i64, cookie_auth_hash: &str) -> bool {
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    let select = SelectQuery::by_pk(AdminUser::SCHEMA, "id", SqlValue::I64(user_id));
    match crate::sql::select_one_row_as_json(&gate.pool, &select, &fields).await {
        Ok(Some(row)) => {
            let current = row
                .get("password_hash")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            session::password_fingerprint(&gate.secret, current) == cookie_auth_hash
        }
        Ok(None) => false,
        Err(_) => true,
    }
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use super::*;
    use crate::sql::sqlx::PgPool;
    use crate::sql::Pool;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    // A lazily-connected pool — these tests exercise the CSRF gate,
    // which runs BEFORE any DB access, so the pool is never queried.
    fn test_state() -> AppState {
        let pool = Pool::Postgres(
            PgPool::connect_lazy("postgres://_:_@127.0.0.1:1/_unused")
                .expect("connect_lazy never fails"),
        );
        let mut config = super::super::urls::Config::default();
        config.session_secret = Some(crate::session::SessionSecret::from_bytes(vec![7u8; 32]));
        AppState {
            pool,
            config: Arc::new(config),
        }
    }

    #[tokio::test]
    async fn get_login_seeds_csrf_cookie_and_form_token() {
        let resp = public_router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("GET should seed a CSRF cookie")
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("rustango_csrf="), "{set_cookie}");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            body.contains(r#"name="_csrf""#),
            "form must carry the token"
        );
    }

    #[tokio::test]
    async fn post_login_without_csrf_token_is_rejected() {
        let resp = public_router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("username=alice&password=secret"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Re-render (200), NOT a 303 redirect — and no session cookie.
        assert_eq!(resp.status(), StatusCode::OK);
        let issued_session = resp.headers().get_all(header::SET_COOKIE).iter().any(|c| {
            c.to_str()
                .map(|s| s.contains(SESSION_COOKIE))
                .unwrap_or(false)
        });
        assert!(
            !issued_session,
            "a CSRF-less POST must not establish a session"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("try again"));
    }
}
