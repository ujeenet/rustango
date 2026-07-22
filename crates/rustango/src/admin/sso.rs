//! Bare-admin SSO login wiring — the `admin-sso` feature.
//!
//! The reusable, admin-INDEPENDENT SSO core (types, `build_provider`,
//! `verified_email`, the oauth2 re-exports, `SSO_FLOW_COOKIE`, …) now
//! lives in [`crate::sso`]. This module keeps only the **bare-admin
//! wiring**: the global-config SSO login router + handlers that build a
//! [`ResolvedSso`] from a [`SsoProvider`](crate::sso::SsoProvider) row,
//! run the handshake, link the verified email to an [`AdminUser`], and
//! mint the admin session cookie.
//!
//! Access is **link-to-existing** — SSO never auto-provisions an admin;
//! an unknown or unverified email is refused.
//!
//! The `pub use crate::sso::*;` re-export below preserves the historical
//! `crate::admin::sso::{build_provider, ResolvedSso, …}` paths for
//! downstream callers (e.g. [`crate::tenancy::sso`]).

// Back-compat: re-export the admin-independent SSO core so the historical
// `crate::admin::sso::*` paths keep resolving. The core moved to
// `crate::sso` (the `sso` feature) to decouple member SSO from the admin.
pub use crate::sso::*;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};

use super::session::{self, AdminSession, SESSION_COOKIE};
use super::urls::AppState;
use super::user::AdminUser;
use crate::core::Model as _; // brings `AdminUser::SCHEMA` into scope

/// Query params on the IdP callback (`?code=…&state=…` or `?error=…`).
#[derive(serde::Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Routes for the bare-admin SSO flow, mounted alongside `/login`.
/// `GET /login/sso/{slug}` starts the handshake for one configured
/// [`SsoProvider`](super::sso_provider::SsoProvider); `.../callback`
/// completes it.
pub(crate) fn sso_router(state: AppState) -> Router {
    Router::new()
        .route("/login/sso/{slug}", get(sso_begin))
        .route("/login/sso/{slug}/callback", get(sso_callback))
        .with_state(state)
}

fn login_path(state: &AppState) -> String {
    let p = &state.config.admin_prefix;
    if p.is_empty() {
        "/login".to_owned()
    } else {
        format!("{p}/login")
    }
}

/// Absolute per-provider callback URL derived from the request host —
/// `{scheme}://{host}{login_path}/sso/{slug}/callback`. Scheme honors
/// `X-Forwarded-Proto`, else `https`.
fn derive_bare_redirect(headers: &HeaderMap, state: &AppState, slug: &str) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("https");
    Some(format!(
        "{scheme}://{host}{}/sso/{slug}/callback",
        login_path(state)
    ))
}

/// Redirect back to the login page with a generic `?sso_error=` marker.
/// Details are logged, never shown to the user.
fn login_error(state: &AppState, code: &str) -> Response {
    Redirect::to(&format!("{}?sso_error={code}", login_path(state))).into_response()
}

fn cookie_attrs(secure: bool) -> &'static str {
    if secure {
        "; Secure"
    } else {
        ""
    }
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

// GET /login/sso/{slug} — start the handshake for one provider.
async fn sso_begin(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(secret) = state.config.session_secret.as_ref() else {
        return login_error(&state, "disabled");
    };
    let Some(redirect_uri) = derive_bare_redirect(&headers, &state, &slug) else {
        return login_error(&state, "config");
    };
    let cfg = match super::sso_provider::resolve_by_slug(&state.pool, &slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(&state, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "begin resolve: {e}");
            return login_error(&state, "config");
        }
    };
    let provider = match build_provider(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "begin: {e}");
            return login_error(&state, "config");
        }
    };
    let (url, flow) = provider.begin();
    let sealed = seal_flow(&flow, secret.key());
    let cookie = format!(
        "{SSO_FLOW_COOKIE}={sealed}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600{s}",
        s = cookie_attrs(state.config.secure_cookies),
    );
    let mut resp = Redirect::to(&url).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

// GET /login/sso/{slug}/callback — finish the handshake, link, mint.
async fn sso_callback(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(params): Query<CallbackParams>,
) -> Response {
    let Some(secret) = state.config.session_secret.as_ref() else {
        return login_error(&state, "disabled");
    };
    if params.error.is_some() {
        return login_error(&state, "denied");
    }
    let (Some(code), Some(cb_state)) = (params.code, params.state) else {
        return login_error(&state, "callback");
    };
    // Recover + verify the sealed flow from its cookie.
    let Some(sealed) = read_cookie(&headers, SSO_FLOW_COOKIE) else {
        return login_error(&state, "expired");
    };
    let flow = match open_flow(&sealed, secret.key()) {
        Ok(f) => f,
        Err(_) => return login_error(&state, "expired"),
    };
    let Some(redirect_uri) = derive_bare_redirect(&headers, &state, &slug) else {
        return login_error(&state, "config");
    };
    let cfg = match super::sso_provider::resolve_by_slug(&state.pool, &slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(&state, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "callback resolve: {e}");
            return login_error(&state, "config");
        }
    };
    let provider = match build_provider(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "callback build: {e}");
            return login_error(&state, "config");
        }
    };
    let normalized = match provider.complete(&flow, &code, &cb_state).await {
        Ok((u, _tokens)) => u,
        Err(e) => {
            tracing::warn!(target: "rustango::admin::sso", "handshake: {e}");
            return login_error(&state, "handshake");
        }
    };
    let email = match verified_email(&normalized) {
        Ok(e) => e.to_ascii_lowercase(),
        Err(_) => return login_error(&state, "unverified"),
    };

    // Link to an existing admin user by email — never auto-provision.
    let Some(user) = find_admin_user_by_email(&state.pool, &email).await else {
        tracing::warn!(target: "rustango::admin::sso", "no admin account for {email}");
        return login_error(&state, "nouser");
    };
    if !user.active {
        return login_error(&state, "inactive");
    }

    // Mint the *existing* admin session bound to the user's stored
    // password hash — identical to a successful password login.
    let auth_hash = session::password_fingerprint(secret, &user.password_hash);
    let cookie_value = session::encode(
        secret,
        AdminSession {
            user_id: user.id,
            username: user.username,
            is_superuser: user.is_superuser,
        },
        &auth_hash,
    );
    let session_cookie = format!(
        "{SESSION_COOKIE}={cookie_value}; Path=/; HttpOnly; SameSite=Lax{s}",
        s = cookie_attrs(state.config.secure_cookies),
    );
    // Clear the transient flow cookie.
    let clear_flow = format!("{SSO_FLOW_COOKIE}=; Path=/; HttpOnly; Max-Age=0");
    let redirect_to = if state.config.admin_prefix.is_empty() {
        "/".to_owned()
    } else {
        state.config.admin_prefix.clone()
    };
    let mut resp = Redirect::to(&redirect_to).into_response();
    if let Ok(v) = HeaderValue::from_str(&session_cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&clear_flow) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// Minimal admin-user identity resolved by email for SSO linking.
struct LinkedAdmin {
    id: i64,
    username: String,
    password_hash: String,
    is_superuser: bool,
    active: bool,
}

/// Look up an [`AdminUser`] by its (lowercased) email. Returns `None`
/// when no row matches — the caller refuses the login (link-to-existing).
async fn find_admin_user_by_email(pool: &crate::sql::Pool, email: &str) -> Option<LinkedAdmin> {
    use crate::core::{SelectQuery, SqlValue};
    let select = SelectQuery::by_pk(
        AdminUser::SCHEMA,
        "email",
        SqlValue::String(email.to_owned()),
    );
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    let row = crate::sql::select_one_row_as_json(pool, &select, &fields)
        .await
        .ok()
        .flatten()?;
    Some(LinkedAdmin {
        id: row.get("id").and_then(serde_json::Value::as_i64)?,
        username: row.get("username").and_then(|v| v.as_str())?.to_owned(),
        password_hash: row
            .get("password_hash")
            .and_then(|v| v.as_str())?
            .to_owned(),
        is_superuser: row
            .get("is_superuser")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        active: row
            .get("active")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}
