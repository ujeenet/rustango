//! Tenant-admin SSO — multi-provider OpenID Connect / social OAuth login
//! (`admin-sso` feature).
//!
//! Reuses the shared handshake core in [`crate::admin::sso`]
//! ([`build_provider`], [`verified_email`], flow sealing) and mints the
//! tenant session (`rustango_tenant_session`) bound to an existing
//! `rustango_users.email` in the tenant's own storage. Access is
//! link-to-existing — SSO never auto-provisions a tenant user.
//!
//! Providers are rows, managed from the admin UI: each tenant's own
//! [`crate::admin::sso_provider::SsoProvider`] table (per-tenant, granular)
//! merged with the registry-wide [`SharedSsoProvider`] set (operator-defined,
//! offered to all tenants). On a slug clash the tenant's row wins. Each
//! provider stores a **secret reference** (e.g. `env://…`) resolved via
//! [`SecretsResolver`] at login time, so the raw secret never sits in a
//! column. `{login}/sso/{slug}` starts the handshake for one provider.

use axum::{
    http::{header, request::Parts, HeaderValue},
    response::{IntoResponse, Redirect, Response},
};

use crate::admin::sso::{
    build_provider, open_flow, parse_scopes, seal_flow, verified_email, ProviderButton,
    ResolvedSso, SsoError, SSO_FLOW_COOKIE,
};
use crate::admin::sso_provider::SsoProvider;
use crate::core::Model as _; // brings `User::SCHEMA` into scope
use crate::sql::Pool;

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
use super::tenant_console::{self, SessionSecret, TenantSessionPayload};

/// A registry-wide SSO provider offered to **every** tenant — the shared
/// counterpart of the per-tenant [`crate::admin::sso_provider::SsoProvider`].
///
/// An operator defines a provider once (in the operator console) and it
/// appears on every tenant's login page. Same fields as the per-tenant
/// model; `scope = "registry"` puts the table in the registry database and
/// hides it from tenant admins (only operators manage the shared set). When
/// a tenant configures a provider with the same `slug`, the tenant's row
/// takes precedence on that tenant's login page.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_shared_sso_providers",
    scope = "registry",
    admin(
        list_display = "slug, label, kind, enabled, sort_order",
        ordering = "sort_order",
        readonly_fields = "created_at, updated_at",
    )
)]
#[allow(dead_code)]
pub struct SharedSsoProvider {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,
    #[rustango(max_length = 64, unique)]
    pub slug: String,
    #[rustango(max_length = 150)]
    pub label: String,
    #[rustango(max_length = 32)]
    pub kind: String,
    #[rustango(max_length = 255)]
    pub issuer_url: Option<String>,
    #[rustango(max_length = 255)]
    pub client_id: String,
    /// The OAuth2 client secret, **encrypted at rest** (see
    /// [`crate::admin::sso_provider::SsoProvider::client_secret`]).
    #[rustango(max_length = 1024)]
    pub client_secret: crate::casts::Cast<crate::casts::EncryptedString>,
    #[rustango(default = "true")]
    pub enabled: bool,
    #[rustango(default = "0")]
    pub sort_order: i32,
    #[rustango(max_length = 255)]
    pub scopes: Option<String>,
    #[rustango(auto_now_add)]
    pub created_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
}

/// Enabled providers for this tenant's login page: the tenant's own
/// [`SsoProvider`] rows merged with the registry-wide [`SharedSsoProvider`]
/// set, **tenant-wins** on a slug clash, sorted by `sort_order`. Each button
/// links to `{login_url}/sso/{slug}`. DB errors degrade to an empty list.
pub(crate) async fn list_enabled(
    tenant_pool: &Pool,
    registry_pool: &Pool,
    routes: &RouteConfig,
) -> Vec<ProviderButton> {
    use crate::sql::FetcherPool as _;
    let tenant_rows: Vec<SsoProvider> = SsoProvider::objects()
        .fetch(tenant_pool)
        .await
        .unwrap_or_default();
    let shared_rows: Vec<SharedSsoProvider> = SharedSsoProvider::objects()
        .fetch(registry_pool)
        .await
        .unwrap_or_default();
    merge_provider_buttons(
        &routes.login_url,
        tenant_rows
            .into_iter()
            .map(|r| (r.slug, r.label, r.sort_order, r.enabled)),
        shared_rows
            .into_iter()
            .map(|r| (r.slug, r.label, r.sort_order, r.enabled)),
    )
}

/// Pure merge: tenant providers first (they **win** on a slug clash), then
/// the registry-wide shared set; disabled rows dropped; result sorted by
/// `sort_order`. Each `(slug, label, sort_order, enabled)`.
fn merge_provider_buttons(
    login_base: &str,
    tenant: impl IntoIterator<Item = (String, String, i32, bool)>,
    shared: impl IntoIterator<Item = (String, String, i32, bool)>,
) -> Vec<ProviderButton> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(i32, ProviderButton)> = Vec::new();
    for (slug, label, sort_order, enabled) in tenant.into_iter().chain(shared) {
        if !enabled {
            continue;
        }
        if seen.insert(slug.clone()) {
            out.push((
                sort_order,
                ProviderButton {
                    login_url: format!("{login_base}/sso/{slug}"),
                    slug,
                    label,
                },
            ));
        }
    }
    out.sort_by_key(|(o, _)| *o);
    out.into_iter().map(|(_, b)| b).collect()
}

/// Resolve one provider by `slug` — the tenant's own table first, then the
/// registry-wide shared set — into a ready-to-build [`ResolvedSso`], with
/// the secret dereferenced by the tenancy [`SecretsResolver`]. `Ok(None)`
/// when no enabled row matches.
async fn resolve_by_slug(
    tenant_pool: &Pool,
    registry_pool: &Pool,
    slug: &str,
    redirect_uri: String,
) -> Result<Option<ResolvedSso>, SsoError> {
    use crate::sql::FetcherPool as _;
    let tenant_row = SsoProvider::objects()
        .filter("slug", slug.to_owned())
        .fetch(tenant_pool)
        .await
        .map_err(|e| SsoError::Config(format!("db: {e}")))?
        .into_iter()
        .find(|r| r.enabled);
    // The secret is stored encrypted at rest and decrypted transparently on
    // load; `into_inner()` yields the plaintext to send to the IdP's token
    // endpoint (over TLS).
    let (kind, issuer_url, client_id, client_secret, scopes) = if let Some(r) = tenant_row {
        (
            r.kind,
            r.issuer_url,
            r.client_id,
            r.client_secret.into_inner(),
            r.scopes,
        )
    } else {
        let shared = SharedSsoProvider::objects()
            .filter("slug", slug.to_owned())
            .fetch(registry_pool)
            .await
            .map_err(|e| SsoError::Config(format!("db: {e}")))?
            .into_iter()
            .find(|r| r.enabled);
        let Some(r) = shared else {
            return Ok(None);
        };
        (
            r.kind,
            r.issuer_url,
            r.client_id,
            r.client_secret.into_inner(),
            r.scopes,
        )
    };
    Ok(Some(ResolvedSso {
        provider: kind,
        issuer_url,
        client_id,
        client_secret,
        redirect_uri,
        scopes: parse_scopes(scopes.as_deref()),
    }))
}

/// Derive the absolute per-provider callback URL for this tenant from the
/// request — tenants are host-based, so the redirect_uri is per-host +
/// per-slug: `{scheme}://{host}{login_url}/sso/{slug}/callback`. Scheme
/// honors `X-Forwarded-Proto` (proxy), else defaults to `https`.
fn derive_redirect(parts: &Parts, routes: &RouteConfig, slug: &str) -> Option<String> {
    let host = parts.headers.get(header::HOST)?.to_str().ok()?;
    let scheme = parts
        .headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("https");
    Some(format!(
        "{scheme}://{host}{}/sso/{slug}/callback",
        routes.login_url
    ))
}

fn login_error(routes: &RouteConfig, code: &str) -> Response {
    Redirect::to(&format!("{}?sso_error={code}", routes.login_url)).into_response()
}

/// `"; Secure"` on the prod tier (HTTPS), empty in dev so local-HTTP SSO
/// works — the framework's session-cookie posture (audit H2), same as
/// the tenant login cookie.
fn secure_suffix() -> &'static str {
    if crate::session::secure_cookies() {
        "; Secure"
    } else {
        ""
    }
}

fn set_cookie(resp: &mut Response, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
}

/// `GET {login_url}/sso/{slug}` — start the tenant handshake for one
/// provider (tenant-owned or shared).
pub(super) async fn tenant_sso_begin(
    slug: &str,
    secret: &SessionSecret,
    tenant_pool: &Pool,
    registry_pool: &Pool,
    routes: &RouteConfig,
    parts: &Parts,
) -> Response {
    let Some(redirect_uri) = derive_redirect(parts, routes, slug) else {
        return login_error(routes, "config");
    };
    let cfg = match resolve_by_slug(tenant_pool, registry_pool, slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(routes, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "begin resolve: {e}");
            return login_error(routes, "config");
        }
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
    let flow_cookie = format!(
        "{SSO_FLOW_COOKIE}={sealed}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600{}",
        secure_suffix()
    );
    let mut resp = Redirect::to(&url).into_response();
    set_cookie(&mut resp, &flow_cookie);
    resp
}

/// `GET {login_url}/sso/{slug}/callback` — finish the handshake, link by
/// email, mint the tenant session.
pub(super) async fn tenant_sso_callback(
    org: &Org,
    slug: &str,
    secret: &SessionSecret,
    tenant_pool: &Pool,
    registry_pool: &Pool,
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
    let Some(redirect_uri) = derive_redirect(parts, routes, slug) else {
        return login_error(routes, "config");
    };
    let cfg = match resolve_by_slug(tenant_pool, registry_pool, slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(routes, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "callback resolve: {e}");
            return login_error(routes, "config");
        }
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
    let session_cookie = format!(
        "{}={cookie_value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{}",
        tenant_console::COOKIE_NAME,
        secure_suffix()
    );
    let clear_flow = format!(
        "{SSO_FLOW_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        secure_suffix()
    );
    let mut resp = Redirect::to(routes.admin_url.as_str()).into_response();
    set_cookie(&mut resp, &session_cookie);
    set_cookie(&mut resp, &clear_flow);
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

#[cfg(test)]
mod tests {
    use super::merge_provider_buttons;

    fn row(slug: &str, label: &str, sort: i32, enabled: bool) -> (String, String, i32, bool) {
        (slug.into(), label.into(), sort, enabled)
    }

    #[test]
    fn tenant_wins_on_slug_clash_and_result_is_sorted() {
        let buttons = merge_provider_buttons(
            "/login",
            // tenant: a clashing "corp" (wins), a disabled one (dropped)
            vec![
                row("corp", "Tenant Corp", 1, true),
                row("off", "Off", 0, false),
            ],
            // shared: same "corp" (loses), a shared-only "global"
            vec![
                row("corp", "Shared Corp", 0, true),
                row("global", "Global", 5, true),
            ],
        );
        let view: Vec<_> = buttons
            .iter()
            .map(|b| (b.slug.as_str(), b.label.as_str(), b.login_url.as_str()))
            .collect();
        assert_eq!(
            view,
            vec![
                ("corp", "Tenant Corp", "/login/sso/corp"), // tenant label wins, sort=1
                ("global", "Global", "/login/sso/global"),  // shared-only, sort=5
            ]
        );
    }

    #[test]
    fn shared_only_when_no_tenant_providers() {
        let buttons = merge_provider_buttons(
            "/admin/login",
            std::iter::empty(),
            vec![row("okta", "Okta", 0, true)],
        );
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].login_url, "/admin/login/sso/okta");
    }
}
