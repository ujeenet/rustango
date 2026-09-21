//! Session auth for the bare admin: `GET`/`POST /login`, `POST /logout`
//! and the gate middleware.
//!
//! Mounted by [`crate::admin::Builder::with_session_auth`]. Every route
//! except `/login` needs a valid session cookie. A missing or expired
//! cookie redirects to `/login` under `state.config.admin_prefix`.
//!
//! Signing uses `crate::session` and password checks use
//! `crate::passwords::verify`: the same primitives tenancy uses. Only
//! the user model and the cookie shape differ.

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

/// Unauthenticated routes: `/login` and `/logout`. Merged into the
/// admin router before the auth middleware, so the login form itself
/// stays reachable.
pub(crate) fn public_router(state: AppState) -> Router {
    Router::new()
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout_submit))
        .with_state(state)
}

/// Routes that sit behind the session middleware, so an unauthenticated
/// visitor can't reach the password-change form. Mounted from
/// [`crate::admin::Builder::build`] when `with_session_auth` is set.
pub(crate) fn protected_router(state: AppState) -> Router {
    let router = Router::new().route(
        "/account/password",
        get(change_password_form).post(change_password_submit),
    );
    // Self-service TOTP enrollment, behind the session gate. Only
    // mounted when the `totp` feature is on.
    #[cfg(feature = "totp")]
    let router = router.route(
        "/account/totp",
        get(totp_enroll_form).post(totp_enroll_submit),
    );
    router.with_state(state)
}

// ============================================================ Login form (GET)

async fn login_form(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    login_response(&state, &headers, None).await
}

/// Render the login page and seed a double-submit CSRF token. The GET
/// sets the cookie (if it is missing) and embeds the matching token as
/// a hidden field, so [`login_submit`] can check it without relying on
/// outer middleware.
async fn login_response(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    error: Option<&str>,
) -> Response {
    use crate::forms::csrf;
    let (token, set_cookie) = csrf::ensure_token(headers, csrf::CSRF_COOKIE);
    let html = render_login_form(state, error, &csrf::csrf_input_html(&token)).await;
    let mut resp = Html(html).into_response();
    if let Some(cookie) = set_cookie {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

async fn render_login_form(state: &AppState, error: Option<&str>, csrf_input: &str) -> String {
    let admin_prefix = &state.config.admin_prefix;
    // One SSO button per enabled `SsoProvider` row.
    #[cfg(feature = "admin-sso")]
    let sso_providers =
        super::sso_provider::list_enabled(&state.pool, &format!("{admin_prefix}/login")).await;
    #[cfg(feature = "admin-sso")]
    let sso_enabled = !sso_providers.is_empty();
    #[cfg(feature = "admin-sso")]
    let sso_providers_json = serde_json::to_value(&sso_providers).unwrap_or_default();
    #[cfg(not(feature = "admin-sso"))]
    let sso_enabled = false;
    #[cfg(not(feature = "admin-sso"))]
    let sso_providers_json = serde_json::Value::Array(Vec::new());
    let ctx = serde_json::json!({
        "title": "Sign in",
        "action": format!("{admin_prefix}/login"),
        "error": error,
        "csrf_input": csrf_input,
        "sso_enabled": sso_enabled,
        "sso_providers": sso_providers_json,
        "admin_title": state
            .config
            .title
            .as_deref()
            .unwrap_or("Rustango Admin"),
        "admin_prefix": admin_prefix,
        "static_url": &state.config.static_url,
        // Show the authenticator-code field when `totp` is compiled in.
        // Users who are not enrolled just leave it blank, so a
        // build-time flag is enough and the login GET needs no lookup.
        "totp_enabled": cfg!(feature = "totp"),
    });
    render_template("login.html", &ctx)
}

// ============================================================ Login form (POST)

#[derive(serde::Deserialize)]
struct LoginInput {
    username: String,
    password: String,
    /// Double-submit CSRF token. Optional, so a missing field re-renders
    /// the form instead of failing the whole POST with a 422.
    #[serde(rename = "_csrf", default)]
    csrf_token: Option<String>,
    /// Authenticator (TOTP) code. Only enrolled users need it. The form
    /// always carries the field, so a 2FA user sends username, password
    /// and code in one step, and turning the feature on does not change
    /// the wire format. Ignored without the `totp` feature or without a
    /// confirmed device.
    #[cfg_attr(not(feature = "totp"), allow(dead_code))]
    #[serde(default)]
    totp_code: Option<String>,
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

    // Check the double-submit CSRF token before any database work or
    // password check. A cross-site POST cannot read the SameSite=Lax
    // cookie to echo the token back, so it fails here.
    if !crate::forms::csrf::verify_form_token(&headers, form.csrf_token.as_deref()) {
        return login_response(
            &state,
            &headers,
            Some("Your session expired or the form was invalid. Please try again."),
        )
        .await;
    }

    // Schema-driven lookup: the bare admin compiles without `tenancy`,
    // so it cannot use tenancy's typed query helpers.
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
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
        // Spend a verify's worth of work on the unknown-user path, so
        // timing does not reveal whether the username exists.
        crate::passwords::verify_dummy(&form.password);
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::InvalidCredentials,
            request: meta.clone(),
        })
        .await;
        return login_response(&state, &headers, Some("Invalid credentials.")).await;
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

    // Per-account brute-force lockout, on by default. The key is scoped
    // (`admin:<id>`) so it cannot collide with operator or tenant ids,
    // and it uses the resolved id, not the raw username, so an attacker
    // cannot lock accounts at will. A locked account stops here, before
    // the password verify.
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
        )
        .await;
    }

    // Verify before the active check, so active and inactive accounts
    // take the same time.
    let password_ok = crate::passwords::verify(&form.password, stored_hash).unwrap_or(false);

    if !is_active {
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::Inactive,
            request: meta.clone(),
        })
        .await;
        // Do **not** reveal that the account exists but is disabled.
        // Use the same generic message as unknown user or wrong
        // password, so the form cannot be used to enumerate accounts.
        // The signal above still records the real reason.
        return login_response(&state, &headers, Some("Invalid credentials.")).await;
    }
    if !password_ok {
        // Count this failure toward the per-account lockout.
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
        return login_response(&state, &headers, Some("Invalid credentials.")).await;
    }

    // Two-factor challenge. A user with a confirmed TOTP device must
    // send a valid code before the session is granted. The password is
    // already verified here; a missing or wrong code re-renders the
    // login form.
    #[cfg(feature = "totp")]
    {
        if let Some(totp_secret) = super::totp_store::confirmed_secret(&state.pool, id).await {
            let code = form.totp_code.as_deref().unwrap_or("").trim();
            // 30s step, 6 digits, ±1 window: the authenticator-app
            // defaults, which allow one step of clock skew.
            if code.is_empty() || !crate::totp::verify(&totp_secret, code, 30, 6, 1) {
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
                    Some("Enter the 6-digit code from your authenticator app."),
                )
                .await;
            }
        }
    }

    // A successful login clears the failure counter and any lock.
    #[cfg(feature = "cache")]
    crate::account_lockout::shared()
        .clear(&format!("admin:{id}"))
        .await;

    // Bind the cookie to a fingerprint of the current password hash, so
    // a password change or reset invalidates it.
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
    // The middleware guarantees a session here. Reaching this handler
    // without one is a bug, so bail loudly.
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

    // Load the row by the session's user_id, so the current password
    // can be verified before the hash is replaced.
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
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

    // Schema-driven UPDATE, so the bare admin compiles without
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

    // This request's cookie holds the OLD password fingerprint, so the
    // gate would sign it out on the next request. Re-issue the cookie
    // with the new fingerprint: this device stays signed in, and every
    // other device's pre-change cookie stops working.
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

// ========================================================== TOTP 2FA enrollment

#[cfg(feature = "totp")]
#[derive(serde::Deserialize)]
struct TotpEnrollInput {
    #[serde(default)]
    totp_code: Option<String>,
    /// Set to `reset=1` to re-enroll an already-enabled account: wipes
    /// the current device and shows a fresh setup.
    #[serde(default)]
    reset: Option<String>,
}

#[cfg(feature = "totp")]
fn render_totp_enroll(
    state: &AppState,
    already_enabled: bool,
    secret_base32: &str,
    otpauth_url: &str,
    error: Option<&str>,
    success: Option<&str>,
) -> String {
    let admin_prefix = &state.config.admin_prefix;
    let ctx = serde_json::json!({
        "title": "Two-factor authentication",
        "action": format!("{admin_prefix}/account/totp"),
        "admin_title": state.config.title.as_deref().unwrap_or("Rustango Admin"),
        "admin_prefix": admin_prefix,
        "static_url": &state.config.static_url,
        "already_enabled": already_enabled,
        "secret_base32": secret_base32,
        "otpauth_url": otpauth_url,
        "error": error,
        "success": success,
    });
    render_template("totp_enroll.html", &ctx)
}

/// Build an `otpauth://` URI for the session user against `secret`.
#[cfg(feature = "totp")]
fn enroll_otpauth(state: &AppState, account: &str, secret: &crate::totp::TotpSecret) -> String {
    let issuer = state.config.title.as_deref().unwrap_or("Rustango Admin");
    crate::totp::otpauth_url(issuer, account, secret)
}

#[cfg(feature = "totp")]
async fn totp_enroll_form(State(state): State<AppState>) -> Response {
    let Some(session) = super::session::current() else {
        return (StatusCode::UNAUTHORIZED, "session required").into_response();
    };
    let _ = super::totp_store::ensure_table(&state.pool).await;
    let device = super::totp_store::device(&state.pool, session.user_id).await;
    if device.as_ref().is_some_and(|d| d.confirmed) {
        return Html(render_totp_enroll(&state, true, "", "", None, None)).into_response();
    }
    // Reuse a pending secret if there is one, else store a fresh
    // unconfirmed one, so a page reload shows the same setup.
    let secret = match device.and_then(|d| crate::totp::TotpSecret::from_base32(&d.secret_base32)) {
        Some(s) => s,
        None => {
            let s = crate::totp::TotpSecret::generate();
            if super::totp_store::start_enrollment(&state.pool, session.user_id, &s)
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not start enrollment",
                )
                    .into_response();
            }
            s
        }
    };
    let otpauth = enroll_otpauth(&state, &session.username, &secret);
    Html(render_totp_enroll(
        &state,
        false,
        &secret.to_base32(),
        &otpauth,
        None,
        None,
    ))
    .into_response()
}

#[cfg(feature = "totp")]
async fn totp_enroll_submit(
    State(state): State<AppState>,
    Form(form): Form<TotpEnrollInput>,
) -> Response {
    let Some(session) = super::session::current() else {
        return (StatusCode::UNAUTHORIZED, "session required").into_response();
    };
    let _ = super::totp_store::ensure_table(&state.pool).await;

    // Re-enroll: wipe + regenerate, then show the fresh setup.
    if form.reset.is_some() {
        let s = crate::totp::TotpSecret::generate();
        let _ = super::totp_store::start_enrollment(&state.pool, session.user_id, &s).await;
        let otpauth = enroll_otpauth(&state, &session.username, &s);
        return Html(render_totp_enroll(
            &state,
            false,
            &s.to_base32(),
            &otpauth,
            None,
            None,
        ))
        .into_response();
    }

    // Confirm: verify the submitted code against the pending secret.
    let Some(device) = super::totp_store::device(&state.pool, session.user_id).await else {
        return Html(render_totp_enroll(
            &state,
            false,
            "",
            "",
            Some("No enrollment in progress — reload the page."),
            None,
        ))
        .into_response();
    };
    let Some(secret) = crate::totp::TotpSecret::from_base32(&device.secret_base32) else {
        return Html(render_totp_enroll(
            &state,
            false,
            "",
            "",
            Some("Stored setup key is invalid — re-enroll."),
            None,
        ))
        .into_response();
    };
    let code = form.totp_code.as_deref().unwrap_or("").trim();
    if code.is_empty() || !crate::totp::verify(&secret, code, 30, 6, 1) {
        let otpauth = enroll_otpauth(&state, &session.username, &secret);
        return Html(render_totp_enroll(
            &state,
            false,
            &device.secret_base32,
            &otpauth,
            Some("That code didn't match. Try again."),
            None,
        ))
        .into_response();
    }
    if super::totp_store::confirm(&state.pool, session.user_id)
        .await
        .is_err()
    {
        return Html(render_totp_enroll(
            &state,
            false,
            "",
            "",
            Some("Could not save — please try again."),
            None,
        ))
        .into_response();
    }
    Html(render_totp_enroll(
        &state,
        true,
        "",
        "",
        None,
        Some("Two-factor authentication is now enabled."),
    ))
    .into_response()
}

// ============================================================ Logout (POST)

async fn logout_submit(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    use crate::signals::auth::{meta_from_headers, send_user_logged_out, UserLoggedOutContext};
    let meta = meta_from_headers(&headers, Some("/logout"));

    // Best-effort decode, so the signal carries the user id and
    // username when the cookie is still valid.
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

/// State for the auth middleware: the signing secret and the login URL
/// to redirect to. Cloned per request; the key stays behind an `Arc` so
/// it is not copied.
#[derive(Clone)]
pub(crate) struct SessionGate {
    pub(crate) secret: Arc<AdminSessionSecret>,
    pub(crate) login_path: String,
    /// When `true`, a non-superuser session gets a 403 page. On by
    /// default for the bare admin.
    pub(crate) require_superuser: bool,
    /// Pool for the per-request password-fingerprint check that rejects
    /// cookies minted before a password change.
    pub(crate) pool: crate::sql::Pool,
}

/// Require a valid session cookie on every admin request. `/login` and
/// the embedded static assets under `/__static__/` pass through.
///
/// A valid session is inserted as `Extension<AdminSession>` so handlers
/// can read the current user. With `gate.require_superuser` set (the
/// bare-admin default), a non-superuser session gets a 403 page.
pub(crate) async fn require_session(
    State(gate): State<SessionGate>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path == gate.login_path || path == "/login" || path.starts_with("/__static__") {
        return next.run(request).await;
    }

    if let Some((mut session, cookie_auth_hash)) = read_session_cookie(&request, &gate.secret) {
        // One lookup per request re-reads the user's live state. It
        // rejects cookies minted before a password change, and re-reads
        // `active` and `is_superuser` from the database instead of
        // trusting the cookie. A deactivated or demoted admin loses
        // access at once, not at cookie expiry.
        match gate_live_check(&gate, session.user_id, &cookie_auth_hash).await {
            // Password changed / user deleted / deactivated → force re-login.
            GateCheck::Reject => return Redirect::to(&gate.login_path).into_response(),
            // Row found + fingerprint matches: trust the LIVE flag.
            GateCheck::Live { is_superuser } => session.is_superuser = is_superuser,
            // Transient DB error: fail open on the fingerprint and
            // active checks (the cookie HMAC and expiry still bound the
            // session) and keep the cookie's `is_superuser`.
            GateCheck::DbError => {}
        }
        if gate.require_superuser && !session.is_superuser {
            // Render a 403 here instead of redirecting to /login. A
            // redirect would loop: login, 403, login again.
            return forbidden_page(&session);
        }
        request.extensions_mut().insert(session.clone());
        // Scope the task-local so `chrome_context`, deep in the render
        // stack, can read the session without every handler passing it
        // down as an argument.
        return super::session::CURRENT_SESSION
            .scope(session, next.run(request))
            .await;
    }

    // No valid session: bounce to the login form with a 303 See Other,
    // so the browser follows with a GET.
    Redirect::to(&gate.login_path).into_response()
}

/// Minimal 403 page for a non-superuser session. Plain HTML with no
/// chrome: rendering the chrome needs this same gate to have passed.
/// The body offers a sign-out button.
fn forbidden_page(session: &AdminSession) -> Response {
    // Inline escape. The gate runs before `next.run`, so the chrome's
    // `render::escape` helper is not reachable here without rebuilding
    // state, and the username must still be escaped.
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

/// Outcome of the gate's per-request liveness lookup.
enum GateCheck {
    /// Row found and the password fingerprint matches. Carries the live
    /// `is_superuser`, so the gate does not trust the cookie's copy.
    Live { is_superuser: bool },
    /// Force a re-login: password changed, user deleted, or account
    /// deactivated.
    Reject,
    /// Transient DB error. The caller fails open on the live checks; the
    /// cookie HMAC and expiry still bound the session.
    DbError,
}

/// Re-read the user's live state in one lookup: the password
/// fingerprint, which rejects cookies minted before a password change,
/// plus live `active` and `is_superuser`.
async fn gate_live_check(gate: &SessionGate, user_id: i64, cookie_auth_hash: &str) -> GateCheck {
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    let select = SelectQuery::by_pk(AdminUser::SCHEMA, "id", SqlValue::I64(user_id));
    match crate::sql::select_one_row_as_json(&gate.pool, &select, &fields).await {
        Ok(Some(row)) => {
            let current = row
                .get("password_hash")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if session::password_fingerprint(&gate.secret, current) != cookie_auth_hash {
                return GateCheck::Reject; // password changed since login
            }
            // `active` defaults to true, as in the login check, so a
            // missing or null column does not lock everyone out. A real
            // `false` revokes the session.
            let active = row.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
            if !active {
                return GateCheck::Reject;
            }
            let is_superuser = row
                .get("is_superuser")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            GateCheck::Live { is_superuser }
        }
        Ok(None) => GateCheck::Reject, // user deleted
        Err(_) => GateCheck::DbError,
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

    // A lazily-connected pool. These tests exercise the CSRF gate,
    // which runs before any DB access, so the pool is never queried.
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
        // Re-render (200), not a 303 redirect, and no session cookie.
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
