//! Tenant-admin SSO — per-`Org` OpenID Connect / social OAuth login
//! (`admin-sso` feature).
//!
//! Reuses the shared handshake core in [`crate::admin::sso`]
//! ([`build_provider`], [`verified_email`], flow sealing) and mints the
//! tenant session (`rustango_tenant_session`) bound to an existing
//! `rustango_users.email` in the tenant's own storage. Access is
//! link-to-existing — SSO never auto-provisions a tenant user.
//!
//! Each tenant brings its own IdP: the provider, client id, issuer, and
//! a **secret reference** live on the `Org` row; the reference is
//! resolved via [`SecretsResolver`] at login time (mirrors
//! `Org.database_url`), so the raw secret never sits in a column.

use axum::{
    http::{header, request::Parts, HeaderValue},
    response::{IntoResponse, Redirect, Response},
};
use cookie::{time::Duration as CookieDuration, Cookie, SameSite};

use crate::admin::sso::{
    build_provider, open_flow, seal_flow, verified_email, ResolvedSso, SSO_FLOW_COOKIE,
};
use crate::core::Model as _; // brings `User::SCHEMA` into scope

/// IdP callback params (`?code=…&state=…` or `?error=…`).
#[derive(serde::Deserialize, Default)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

use super::auth::User;
use super::org::Org;
use super::routes::RouteConfig;
use super::secrets::{ChainSecretsResolver, SecretsResolver};
use super::tenant_console::{self, SessionSecret, TenantSessionPayload};

/// Path suffixes (relative to `routes.login_url`) for the two SSO routes.
pub(super) const SSO_BEGIN_SUFFIX: &str = "/sso";
pub(super) const SSO_CALLBACK_SUFFIX: &str = "/sso/callback";

/// Read the per-`Org` SSO config when enabled + complete.
/// Returns `(provider, issuer_url, client_id, secret_ref)`.
fn org_sso(org: &Org) -> Option<(String, Option<String>, String, String)> {
    if !org.sso_enabled {
        return None;
    }
    Some((
        org.sso_provider.clone()?,
        org.sso_issuer_url.clone(),
        org.sso_client_id.clone()?,
        org.sso_secret_ref.clone()?,
    ))
}

/// Derive the absolute callback URL for this tenant from the request —
/// tenants are host-based, so the redirect_uri is per-host:
/// `{scheme}://{host}{login_url}/sso/callback`. Scheme honors
/// `X-Forwarded-Proto` (proxy), else defaults to `https`.
fn derive_redirect(parts: &Parts, routes: &RouteConfig) -> Option<String> {
    let host = parts.headers.get(header::HOST)?.to_str().ok()?;
    let scheme = parts
        .headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("https");
    Some(format!(
        "{scheme}://{host}{}{SSO_CALLBACK_SUFFIX}",
        routes.login_url
    ))
}

/// Build the resolved provider config for this tenant, dereferencing the
/// stored secret reference.
async fn resolve(org: &Org, parts: &Parts, routes: &RouteConfig) -> Result<ResolvedSso, Response> {
    let Some((provider, issuer_url, client_id, secret_ref)) = org_sso(org) else {
        return Err(login_error(routes, "disabled"));
    };
    let Some(redirect_uri) = derive_redirect(parts, routes) else {
        return Err(login_error(routes, "config"));
    };
    let client_secret = match ChainSecretsResolver::standard().resolve(&secret_ref).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "secret resolve: {e}");
            return Err(login_error(routes, "config"));
        }
    };
    Ok(ResolvedSso {
        provider,
        issuer_url,
        client_id,
        client_secret,
        redirect_uri,
    })
}

fn login_error(routes: &RouteConfig, code: &str) -> Response {
    Redirect::to(&format!("{}?sso_error={code}", routes.login_url)).into_response()
}

fn set_cookie(resp: &mut Response, cookie: Cookie<'_>) {
    if let Ok(v) = HeaderValue::from_str(&cookie.to_string()) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
}

/// `GET {login_url}/sso` — start the tenant handshake.
pub(super) async fn tenant_sso_begin(
    org: &Org,
    secret: &SessionSecret,
    routes: &RouteConfig,
    parts: &Parts,
) -> Response {
    let cfg = match resolve(org, parts, routes).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let provider = match build_provider(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "begin build: {e}");
            return login_error(routes, "config");
        }
    };
    let (url, flow) = provider.begin();
    let sealed = seal_flow(&flow, secret.key());
    let flow_cookie = Cookie::build((SSO_FLOW_COOKIE, sealed))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(600))
        .build();
    let mut resp = Redirect::to(&url).into_response();
    set_cookie(&mut resp, flow_cookie);
    resp
}

/// `GET {login_url}/sso/callback` — finish the handshake, link by email,
/// mint the tenant session.
pub(super) async fn tenant_sso_callback(
    org: &Org,
    secret: &SessionSecret,
    tenant_pool: &crate::sql::Pool,
    routes: &RouteConfig,
    parts: &Parts,
) -> Response {
    let params: CallbackParams =
        serde_urlencoded::from_str(parts.uri.query().unwrap_or("")).unwrap_or_default();
    if params.error.is_some() {
        return login_error(routes, "denied");
    }
    let (Some(code), Some(cb_state)) = (params.code, params.state) else {
        return login_error(routes, "callback");
    };
    let Some(sealed) = read_flow_cookie(parts) else {
        return login_error(routes, "expired");
    };
    let flow = match open_flow(&sealed, secret.key()) {
        Ok(f) => f,
        Err(_) => return login_error(routes, "expired"),
    };
    let cfg = match resolve(org, parts, routes).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let provider = match build_provider(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "callback build: {e}");
            return login_error(routes, "config");
        }
    };
    let normalized = match provider.complete(&flow, &code, &cb_state).await {
        Ok((u, _t)) => u,
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::sso", "handshake: {e}");
            return login_error(routes, "handshake");
        }
    };
    let email = match verified_email(&normalized) {
        Ok(e) => e.to_ascii_lowercase(),
        Err(_) => return login_error(routes, "unverified"),
    };

    let Some((uid, active)) = find_tenant_user_by_email(tenant_pool, &email).await else {
        tracing::warn!(target: "rustango::tenancy::sso", "no tenant user for {email} in {}", org.slug);
        return login_error(routes, "nouser");
    };
    if !active {
        return login_error(routes, "inactive");
    }

    // Mint the tenant session — same shape as a password login.
    let ttl = i64::try_from(routes.tenant_session_ttl.as_secs())
        .unwrap_or(tenant_console::SESSION_TTL_SECS);
    let payload = TenantSessionPayload::new(uid, &org.slug, ttl);
    let cookie_value = tenant_console::encode(secret, &payload);
    let session_cookie = Cookie::build((tenant_console::COOKIE_NAME, cookie_value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(ttl))
        .build();
    let clear_flow = Cookie::build((SSO_FLOW_COOKIE, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(0))
        .build();
    let mut resp = Redirect::to(routes.admin_url.as_str()).into_response();
    set_cookie(&mut resp, session_cookie);
    set_cookie(&mut resp, clear_flow);
    resp
}

fn read_flow_cookie(parts: &Parts) -> Option<String> {
    let raw = parts.headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == SSO_FLOW_COOKIE)
        .map(|(_, v)| v.to_owned())
}

/// Look up a tenant user's `(id, active)` by lowercased email in the
/// tenant's scoped pool. `None` when no row matches (link-to-existing).
async fn find_tenant_user_by_email(pool: &crate::sql::Pool, email: &str) -> Option<(i64, bool)> {
    use crate::core::{SelectQuery, SqlValue};
    let select = SelectQuery::by_pk(User::SCHEMA, "email", SqlValue::String(email.to_owned()));
    let fields: Vec<&'static crate::core::FieldSchema> = User::SCHEMA.fields.iter().collect();
    let row = crate::sql::select_one_row_as_json(pool, &select, &fields)
        .await
        .ok()
        .flatten()?;
    let id = row.get("id").and_then(serde_json::Value::as_i64)?;
    let active = row
        .get("active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Some((id, active))
}
