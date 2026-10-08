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
    // Logout needs no session, but a forged one must still be refused.
    let logout = Router::new()
        .route("/logout", post(logout_submit))
        .route_layer(crate::forms::csrf::layer());
    Router::new()
        .route("/login", get(login_form).post(login_submit))
        .merge(logout)
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

async fn login_form(
    State(state): State<AppState>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
) -> Response {
    login_response(&state, &extensions, &headers, None).await
}

/// Render the login page and seed a double-submit CSRF token. The GET
/// sets the cookie (if it is missing) and embeds the matching token as
/// a hidden field, so [`login_submit`] can check it without relying on
/// outer middleware.
async fn login_response(
    state: &AppState,
    extensions: &axum::http::Extensions,
    headers: &axum::http::HeaderMap,
    error: Option<&str>,
) -> Response {
    login_page(state, headers, error, false).await
}

/// The authenticator-code step an SSO sign-in owes (#2249).
#[cfg(all(feature = "admin-sso", feature = "totp"))]
pub(super) async fn sso_totp_response(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    error: Option<&str>,
) -> Response {
    login_page(state, headers, error, true).await
}

/// `totp_step` renders only the code field, posting to the SSO step.
async fn login_page(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    error: Option<&str>,
    totp_step: bool,
) -> Response {
    use crate::forms::csrf;
    // Under `protect_with_csrf` the outer layer already chose the token (#2131).
    let (token, set_cookie) = match super::session::current_csrf_token() {
        Some(token) => (token, None),
        None => csrf::ensure_token_under_layer(headers, extensions),
    };
    let html = render_login_form(state, error, &csrf::csrf_input_html(&token), totp_step).await;
    let mut resp = Html(html).into_response();
    if let Some(cookie) = set_cookie {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

async fn render_login_form(
    state: &AppState,
    error: Option<&str>,
    csrf_input: &str,
    totp_step: bool,
) -> String {
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
        "action": if totp_step {
            format!("{admin_prefix}/login/sso-totp")
        } else {
            format!("{admin_prefix}/login")
        },
        "totp_step": totp_step,
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
    ip: crate::login_throttle::ClientIp,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginInput>,
) -> Response {
    use crate::login_throttle::{LoginRefused, LoginScope};
    use crate::signals::auth::{
        meta_from_parts, send_user_logged_in, send_user_login_failed, AuthFailureReason,
        UserLoggedInContext, UserLoginFailedContext,
    };
    let meta = meta_from_parts(&extensions, &headers, Some("/login"));

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
    if !crate::forms::csrf::verify_form_token_under_layer(
        &headers,
        &extensions,
        form.csrf_token.as_deref(),
    ) {
        return login_response(
            &state,
            &extensions,
            &headers,
            Some("Your session expired or the form was invalid. Please try again."),
        )
        .await;
    }

    // Rate limits and the account lock, before the lookup, so the
    // answer is the same whether or not the username exists.
    let mut attempt = match crate::login_throttle::shared()
        .begin(&LoginScope::Admin, &ip, &form.username)
        .await
    {
        Ok(a) => a,
        Err(refused) => return refused.into_response(),
    };

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
        if crate::passwords::verify_dummy_async(&form.password)
            .await
            .is_err()
        {
            return LoginRefused::Busy.into_response();
        }
        attempt.failed().await;
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::InvalidCredentials,
            request: meta.clone(),
        })
        .await;
        return login_response(&state, &extensions, &headers, Some("Invalid credentials.")).await;
    };
    let stored_name = row
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or(form.username.as_str());
    if let Err(refused) = attempt.resolve(stored_name).await {
        return refused.into_response();
    }
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

    // Verify before the active check, so active and inactive accounts
    // take the same time.
    let password_ok = match crate::passwords::verify_async(&form.password, stored_hash).await {
        Ok(ok) => ok,
        Err(crate::passwords::PasswordError::Busy) => return LoginRefused::Busy.into_response(),
        Err(_) => false,
    };

    if !is_active {
        attempt.failed().await;
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
        return login_response(&state, &extensions, &headers, Some("Invalid credentials.")).await;
    }
    if !password_ok {
        attempt.failed().await;
        send_user_login_failed(UserLoginFailedContext {
            source: "admin",
            attempted_username: Some(form.username.clone()),
            reason: AuthFailureReason::InvalidCredentials,
            request: meta.clone(),
        })
        .await;
        return login_response(&state, &extensions, &headers, Some("Invalid credentials.")).await;
    }

    // Two-factor challenge. A user with a confirmed TOTP device must
    // send a valid code before the session is granted. The password is
    // already verified here; a missing or wrong code re-renders the
    // login form.
    #[cfg(feature = "totp")]
    {
        // Fail closed. `confirmed_secret` cannot tell "no device" from
        // "could not read", and reading the second as the first grants
        // the session on the password alone (#1644).
        let enrolled = match super::totp_store::confirmed_secret_checked(&state.pool, id).await {
            Ok(secret) => secret,
            Err(e) => {
                tracing::error!(
                    target: "rustango::admin",
                    user_id = id,
                    error = %e,
                    "cannot read the TOTP device; refusing the login rather \
                     than treating it as no second factor",
                );
                send_user_login_failed(UserLoginFailedContext {
                    source: "admin",
                    attempted_username: Some(form.username.clone()),
                    reason: AuthFailureReason::InvalidCredentials,
                    request: meta.clone(),
                })
                .await;
                return login_response(&state, &extensions, &headers, Some("Invalid credentials."))
                    .await;
            }
        };
        if let Some(totp_secret) = enrolled {
            let code = form.totp_code.as_deref().unwrap_or("").trim();
            // Single use: a replayed code fails like a wrong one. A
            // store error fails closed.
            let accepted = !code.is_empty()
                && super::totp_store::redeem_code(&state.pool, id, &totp_secret, code)
                    .await
                    .unwrap_or(false);
            if !accepted {
                // A wrong code counts; a missing one is just the prompt
                // and spends no limit tokens (#1748).
                if code.is_empty() {
                    attempt.prompted().await;
                } else {
                    attempt.failed().await;
                }
                send_user_login_failed(UserLoginFailedContext {
                    source: "admin",
                    attempted_username: Some(form.username.clone()),
                    reason: AuthFailureReason::InvalidCredentials,
                    request: meta.clone(),
                })
                .await;
                return login_response(
                    &state,
                    &extensions,
                    &headers,
                    Some("Enter the 6-digit code from your authenticator app."),
                )
                .await;
            }
        }
    }

    // A successful login clears the failure counter and any lock.
    attempt.succeeded().await;
    let stored_hash = &crate::passwords::upgrade_stored_hash(
        &state.pool,
        AdminUser::SCHEMA,
        id,
        &form.password,
        stored_hash,
    )
    .await;

    // Bind the cookie to a fingerprint of the current password hash, so
    // a password change or reset invalidates it.
    let auth_hash = crate::session::PasswordFingerprint::of(&secret, stored_hash);
    let cookie_value = session::encode(
        &secret,
        AdminSession::new(id, form.username.clone(), is_superuser),
        &auth_hash,
        sessions_revoked_at(&row).unwrap_or_default(),
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
    ip: crate::login_throttle::ClientIp,
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
    if let Err(e) = crate::password_validators::check_builtin_form_password(&form.new_password) {
        return Html(render_change_password_form(&state, None, Some(&e.message))).into_response();
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
    let username = row
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let verify = async {
        match crate::passwords::verify_async(&form.current_password, stored_hash).await {
            Ok(ok) => Ok(ok),
            Err(crate::passwords::PasswordError::Busy) => {
                Err(crate::login_throttle::LoginRefused::Busy)
            }
            Err(_) => Ok(false),
        }
    };
    let ok = match crate::login_throttle::shared()
        .verify_current_password(
            &crate::login_throttle::LoginScope::Admin,
            &ip,
            username,
            verify,
        )
        .await
    {
        Ok(ok) => ok,
        Err(refused) => return refused.into_response(),
    };
    if !ok {
        return Html(render_change_password_form(
            &state,
            None,
            Some("Current password is incorrect."),
        ))
        .into_response();
    }

    let new_hash = match crate::passwords::hash_async(&form.new_password).await {
        Ok(h) => h,
        Err(crate::passwords::PasswordError::Busy) => {
            return crate::login_throttle::LoginRefused::Busy.into_response()
        }
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
        let auth_hash = crate::session::PasswordFingerprint::of(secret, &new_hash);
        let cookie_value = session::encode(
            secret,
            AdminSession::new(
                session.user_id,
                session.username.clone(),
                session.is_superuser,
            ),
            &auth_hash,
            sessions_revoked_at(&row).unwrap_or_default(),
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
    /// Set to `reset=1` with `totp_code` from the current device (#1776) to
    /// re-enroll; the current device stays active until the new one is confirmed.
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
        "csrf_input": super::helpers::current_csrf_input(),
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
    // A re-enroll's pending key is shown only in the reset response, so
    // a session alone cannot read it back (#1756).
    if device.as_ref().is_some_and(|d| d.confirmed) {
        return Html(render_totp_enroll(&state, true, "", "", None, None)).into_response();
    }
    // Reuse a pending secret if there is one, else store a fresh
    // unconfirmed one, so a page reload shows the same setup.
    let pending = device
        .as_ref()
        .and_then(super::totp_store::AdminTotp::pending_secret);
    let secret = match pending {
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

/// Report a code check to the login gate. Only a wrong, non-empty code
/// counts; `None` (no check made) gives the tokens back.
#[cfg(feature = "totp")]
async fn settle_code(attempt: crate::login_throttle::LoginAttempt, ok: Option<bool>, code: &str) {
    match (ok, code.is_empty()) {
        (Some(true), _) => attempt.succeeded().await,
        (Some(false), false) => attempt.failed().await,
        _ => attempt.prompted().await,
    }
}

/// Re-enroll: a current code first, then stage a fresh secret.
#[cfg(feature = "totp")]
async fn totp_reenroll(
    state: &AppState,
    session: &super::session::AdminSession,
    attempt: crate::login_throttle::LoginAttempt,
    code: &str,
) -> Response {
    use super::totp_store::Reenroll;
    let s = crate::totp::TotpSecret::generate();
    let started =
        super::totp_store::start_reenrollment(&state.pool, session.user_id, code, &s).await;
    let ok = match &started {
        Ok(Reenroll::Verified) => Some(true),
        Ok(Reenroll::Refused) => Some(false),
        _ => None,
    };
    settle_code(attempt, ok, code).await;
    let refused = match started {
        Ok(Reenroll::Refused) => Some("Enter a current code from your authenticator to re-enroll."),
        Ok(_) => None,
        Err(_) => Some("Could not start re-enrollment — please try again."),
    };
    if let Some(msg) = refused {
        return Html(render_totp_enroll(state, true, "", "", Some(msg), None)).into_response();
    }
    let otpauth = enroll_otpauth(state, &session.username, &s);
    Html(render_totp_enroll(
        state,
        false,
        &s.to_base32(),
        &otpauth,
        None,
        None,
    ))
    .into_response()
}

#[cfg(feature = "totp")]
async fn totp_enroll_submit(
    State(state): State<AppState>,
    ip: crate::login_throttle::ClientIp,
    Form(form): Form<TotpEnrollInput>,
) -> Response {
    let Some(session) = super::session::current() else {
        return (StatusCode::UNAUTHORIZED, "session required").into_response();
    };
    let _ = super::totp_store::ensure_table(&state.pool).await;

    // Every code here goes through the login gate, so none can be brute-forced.
    let attempt = match crate::login_throttle::shared()
        .begin(
            &crate::login_throttle::LoginScope::Admin,
            &ip,
            &session.username,
        )
        .await
    {
        Ok(a) => a,
        Err(refused) => return refused.into_response(),
    };

    let code = form.totp_code.as_deref().unwrap_or("").trim();
    if form.reset.is_some() {
        return totp_reenroll(&state, &session, attempt, code).await;
    }

    // Confirm: verify the submitted code against the pending secret.
    let device = super::totp_store::device(&state.pool, session.user_id).await;
    let reenroll = device.as_ref().is_some_and(|d| d.confirmed);
    let pending = device
        .as_ref()
        .and_then(super::totp_store::AdminTotp::pending_secret);
    let Some(secret) = pending else {
        settle_code(attempt, None, code).await;
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
    // Confirms and redeems in one write, so the confirming code cannot
    // sign in again and a failed write does not burn it.
    let confirmed = if code.is_empty() {
        Ok(false)
    } else {
        super::totp_store::confirm_with_code(&state.pool, session.user_id, &secret, code).await
    };
    settle_code(attempt, confirmed.as_ref().ok().copied(), code).await;
    if matches!(confirmed, Ok(false)) {
        // Never echo a re-enroll's key: that would hand it to any session.
        let (key, otpauth) = if reenroll {
            (String::new(), String::new())
        } else {
            let url = enroll_otpauth(&state, &session.username, &secret);
            (secret.to_base32(), url)
        };
        return Html(render_totp_enroll(
            &state,
            false,
            &key,
            &otpauth,
            Some("That code didn't match. Try again."),
            None,
        ))
        .into_response();
    }
    if confirmed.is_err() {
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

async fn logout_submit(
    State(state): State<AppState>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
) -> Response {
    use crate::signals::auth::{meta_from_parts, send_user_logged_out, UserLoggedOutContext};
    let meta = meta_from_parts(&extensions, &headers, Some("/logout"));

    let decoded = state.config.session_secret.as_ref().and_then(|secret| {
        let val = crate::cookies::cookie_from_headers(&headers, SESSION_COOKIE)?;
        Some((secret, session::decode_full(secret, val)?))
    });
    // Best-effort, so the signal carries the user id and username when
    // the cookie is still valid.
    let (user_id, username) = decoded.as_ref().map_or((None, None), |(_, (s, _))| {
        (Some(s.user_id), Some(s.username.clone()))
    });
    // End the user's sessions everywhere; only a live cookie may (#1855).
    if let Some((secret, (sess, auth))) = &decoded {
        let revoked = match live_check(&state.pool, secret, sess.user_id, auth).await {
            GateCheck::Live {
                sessions_revoked_at,
                ..
            } => crate::session::revoke_sessions::<AdminUser>(
                &state.pool,
                sess.user_id,
                sessions_revoked_at,
                auth.iat,
            )
            .await
            .map(drop)
            .map_err(|e| e.to_string()),
            GateCheck::Reject => Ok(()),
            // Never report a logout that did not happen.
            GateCheck::DbError => Err("session lookup failed".to_owned()),
        };
        if let Err(e) = revoked {
            tracing::warn!(target: "rustango::admin", error = %e, "logout revoke failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "logout failed").into_response();
        }
    }

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
    /// The 403 page's sign-out target, under the admin prefix.
    pub(crate) logout_path: String,
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

    if let Some((mut session, cookie_auth)) = read_session_cookie(&request, &gate.secret) {
        // One lookup per request re-reads the user's live state. It
        // rejects cookies minted before a password change, and re-reads
        // `active` and `is_superuser` from the database instead of
        // trusting the cookie. A deactivated or demoted admin loses
        // access at once, not at cookie expiry.
        match live_check(&gate.pool, &gate.secret, session.user_id, &cookie_auth).await {
            // Password changed / logged out / user deleted / deactivated → force re-login.
            GateCheck::Reject => return Redirect::to(&gate.login_path).into_response(),
            // Row found + fingerprint matches: trust the LIVE flag.
            GateCheck::Live { is_superuser, .. } => session.is_superuser = is_superuser,
            // Transient DB error: fail open on the fingerprint and
            // active checks (the cookie HMAC and expiry still bound the
            // session) and keep the cookie's `is_superuser`.
            GateCheck::DbError => {}
        }
        if gate.require_superuser && !session.is_superuser {
            // Render a 403 here instead of redirecting to /login. A
            // redirect would loop: login, 403, login again.
            return forbidden_page(&session, &gate.logout_path);
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
fn forbidden_page(session: &AdminSession, logout_path: &str) -> Response {
    let username = crate::text::html_escape(&session.username);
    let logout_path = crate::text::html_escape(logout_path);
    let nonce = crate::csp_nonce::nonce_attr();
    let body = format!(
        "<!doctype html>\
         <html><head><title>Forbidden</title>\
         <style{nonce}>body{{font-family:system-ui;max-width:42em;margin:4em auto;padding:0 1em;line-height:1.5}}\
         h1{{font-size:1.4em}}\
         .meta{{color:#666;font-size:.9em}}\
         </style></head><body>\
         <h1>403 — Admin access required</h1>\
         <p>You are signed in as <strong>{username}</strong>, but only \
         superusers can use the admin.</p>\
         <p class=\"meta\">Ask your administrator to grant superuser \
         status, or sign out below if this isn't the account you \
         intended to use.</p>\
         <form method=\"post\" action=\"{logout_path}\">\
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
) -> Option<(AdminSession, session::CookieAuth)> {
    let val = crate::cookies::cookie_from_headers(req.headers(), SESSION_COOKIE)?;
    session::decode_full(secret, val)
}

/// Outcome of the gate's per-request liveness lookup.
enum GateCheck {
    /// Row found and the session survives. Carries the live `is_superuser`,
    /// so the gate does not trust the cookie's copy, and the logout cut-off.
    Live {
        is_superuser: bool,
        sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// Force a re-login: password changed, logged out, user deleted, or
    /// account deactivated.
    Reject,
    /// Transient DB error. The caller fails open on the live checks; the
    /// cookie HMAC and expiry still bound the session.
    DbError,
}

/// The row's logout cut-off; `Err` when it is set but unreadable.
fn sessions_revoked_at(
    row: &serde_json::Value,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, ()> {
    match row.get(crate::session::SESSIONS_REVOKED_AT) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| Some(d.with_timezone(&chrono::Utc)))
            .ok_or(()),
    }
}

/// Re-read the user's live state in one lookup: the password
/// fingerprint and logout cut-off, which reject cookies minted before a
/// password change or logout, plus live `active` and `is_superuser`.
async fn live_check(
    pool: &crate::sql::Pool,
    secret: &AdminSessionSecret,
    user_id: i64,
    cookie: &session::CookieAuth,
) -> GateCheck {
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    let select = SelectQuery::by_pk(AdminUser::SCHEMA, "id", SqlValue::I64(user_id));
    match crate::sql::select_one_row_as_json(pool, &select, &fields).await {
        Ok(Some(row)) => {
            let current = row
                .get("password_hash")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let Ok(revoked_at) = sessions_revoked_at(&row) else {
                return GateCheck::Reject;
            };
            if !crate::session::session_survives(
                secret,
                &cookie.auth_hash,
                cookie.iat,
                current,
                None,
                revoked_at,
            ) {
                return GateCheck::Reject; // password changed or logged out since login
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
            GateCheck::Live {
                is_superuser,
                sessions_revoked_at: revoked_at,
            }
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

#[cfg(all(test, feature = "sqlite"))]
mod outer_csrf_tests {
    use super::*;
    use crate::forms::csrf::{with_config, CsrfConfig};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    /// #2160 — under a custom cookie name the GET seeds that cookie and
    /// the POST passes both the outer layer and the login check.
    #[tokio::test]
    async fn login_round_trip_uses_the_outer_layers_cookie() {
        let pool = crate::sql::sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let mut config = super::super::urls::Config::default();
        config.session_secret = Some(crate::session::SessionSecret::from_bytes(vec![7u8; 32]));
        let state = AppState {
            pool: crate::sql::Pool::Sqlite(pool),
            config: Arc::new(config),
        };
        let app = public_router(state).layer(with_config(CsrfConfig {
            cookie_name: "outer_csrf".into(),
            ..CsrfConfig::default()
        }));

        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookies: Vec<String> = get
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|c| c.to_str().unwrap().split(';').next().unwrap().to_owned())
            .collect();
        assert_eq!(cookies.len(), 1, "{cookies:?}");
        assert!(cookies[0].starts_with("outer_csrf="), "{cookies:?}");
        let body = axum::body::to_bytes(get.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        let token = body
            .split(r#"name="_csrf" value=""#)
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("form token")
            .to_owned();

        let post = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::COOKIE, cookies.join("; "))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "username=nobody&password=wrong&_csrf={token}"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post.status(), StatusCode::OK);
        let body = axum::body::to_bytes(post.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("Invalid credentials."), "{body}");
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::*;

    /// The 403 page signs out under the admin prefix (#1916).
    #[tokio::test]
    async fn forbidden_page_signs_out_under_the_prefix() {
        let session = AdminSession::new(1, "u", false);
        let res = forbidden_page(&session, "/adm/logout");
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains(r#"action="/adm/logout""#), "{body}");
    }

    /// #1703 — its inline style carries the request's CSP nonce.
    #[tokio::test]
    async fn forbidden_page_style_carries_the_nonce() {
        let session = AdminSession::new(1, "u", false);
        let res =
            crate::csp_nonce::scoped("N0nce", async { forbidden_page(&session, "/logout") }).await;
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        crate::testkit::assert_strict_csp_html(
            &String::from_utf8_lossy(&body),
            "N0nce",
            "403 page",
        );
    }
}
