//! Tenant-admin SSO — multi-provider OpenID Connect / social OAuth login
//! (`admin-sso` feature).
//!
//! Reuses the shared handshake core in [`crate::admin::sso`]
//! ([`build_provider`], flow sealing) and mints the tenant session
//! (`rustango_tenant_session`) for the tenant user linked to the IdP
//! subject ([`crate::sso::link`]). Sign-up follows the provider's `signup_mode` (default closed).
//!
//! Providers are rows, managed from the admin UI: each tenant's own
//! [`crate::admin::sso_provider::SsoProvider`] table (per-tenant, granular)
//! merged with the registry-wide [`SharedSsoProvider`] set (operator-defined,
//! offered to all tenants). On a slug clash the tenant's row wins. Each
//! provider's client secret is stored encrypted (`Cast<EncryptedString>`)
//! and decrypted only at login. `{login}/sso/{slug}` starts the handshake
//! for one provider.

use axum::{
    http::{header, request::Parts, HeaderValue},
    response::{IntoResponse, Redirect, Response},
};

use crate::admin::sso::{
    open_flow, seal_flow, FlowPurpose, FlowScope, ProviderButton, SsoError, SSO_FLOW_COOKIE,
};
use crate::admin::sso_provider::SsoProvider;
use crate::query::QuerySet;
use crate::signals::auth::{
    meta_from_parts, send_user_logged_in, AuthFailureReason, UserLoggedInContext,
};
use crate::sql::Pool;
use crate::sso::build_provider_for;
use crate::sso::link::{signal_refused, LinkSource, ProviderKey};
use crate::sso::policy::{
    admit, refuse, AdmitRequest, Denied, EffectivePolicy, Identity, PolicyError, RefusalContext,
    SharedOverride, Surface, TenantSettings,
};
use crate::sso::provider::{has_slug, load_rows, resolve_row, ResolvedProvider};

/// The auth-signal `source` for a tenant console SSO sign-in.
const SIGNAL_SOURCE: &str = "tenant_admin_sso";

/// The `?sso_error=` of every refused sign-in: the rule stays server-side.
pub(crate) const REFUSED: &str = "nouser";

/// IdP callback params (`?code=…&state=…` or `?error=…`).
#[derive(serde::Deserialize, Default)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

use super::member_auth;
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
        formfield_overrides = "client_secret: password",
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
    /// See [`SsoProvider::allow_email_link`].
    #[rustango(default = "false")]
    pub allow_email_link: bool,
    // The access policy: see the same fields on `SsoProvider`.
    #[rustango(max_length = 2000)]
    pub allowed_email_domains: Option<String>,
    #[rustango(max_length = 2000)]
    pub blocked_email_domains: Option<String>,
    #[rustango(max_length = 1000)]
    pub allowed_idp_tenants: Option<String>,
    #[rustango(max_length = 1000)]
    pub allowed_hosted_domains: Option<String>,
    #[rustango(default = "'{}'")]
    pub required_claims: serde_json::Value,
    #[rustango(default = "true")]
    pub require_verified_email: bool,
    #[rustango(
        max_length = 16,
        choices = "closed:Closed, invite_only:Invite only, allowlist:Allowlist, open:Open"
    )]
    pub signup_mode: Option<String>,
    #[rustango(max_length = 1000)]
    pub default_roles: Option<String>,
    #[rustango(default = "'{}'")]
    pub role_map: serde_json::Value,
    #[rustango(max_length = 255)]
    pub role_claims: Option<String>,
    #[rustango(default = "false")]
    pub sync_roles_on_login: bool,
    #[rustango(
        max_length = 24,
        default = "'email_local'",
        choices = "email_local:Email local part, email:Email, preferred_username:Preferred username"
    )]
    pub username_from: String,
    #[rustango(
        max_length = 16,
        default = "'suffix'",
        choices = "suffix:Add a suffix, refuse:Refuse"
    )]
    pub username_clash: String,
    #[rustango(default = "false")]
    pub sync_profile: bool,
    #[rustango(default = "false")]
    pub allow_email_link_staff: bool,
    #[rustango(default = "false")]
    pub sso_only: bool,
    pub auto_deactivate_after_days: Option<i32>,
    pub session_ttl_secs: Option<i64>,
    pub superuser_session_ttl_secs: Option<i64>,
    #[rustango(auto_now_add)]
    pub created_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
}

inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_shared_sso_providers",
        derive: crate::sso::provider::admin_encrypt_secret,
    }
}

inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_shared_sso_providers",
        derive: crate::sso::policy::admin_normalise,
    }
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
    let tenant_rows = load_rows(QuerySet::<SsoProvider>::new(), tenant_pool)
        .await
        .unwrap_or_default();
    let shared_rows = load_rows(QuerySet::<SharedSsoProvider>::new(), registry_pool)
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

/// Pure merge: tenant providers first (they **win** on a slug clash, a
/// disabled one too), then the registry-wide shared set; disabled rows
/// dropped; result sorted by `sort_order`. Each `(slug, label, sort_order, enabled)`.
fn merge_provider_buttons(
    login_base: &str,
    tenant: impl IntoIterator<Item = (String, String, i32, bool)>,
    shared: impl IntoIterator<Item = (String, String, i32, bool)>,
) -> Vec<ProviderButton> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(i32, ProviderButton)> = Vec::new();
    for (slug, label, sort_order, enabled) in tenant.into_iter().chain(shared) {
        if seen.insert(slug.clone()) && enabled {
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
/// registry-wide shared set — with its link key. `Ok(None)` when no enabled
/// row matches, or the tenant's own row for the slug is disabled.
async fn resolve_by_slug(
    tenant_pool: &Pool,
    registry_pool: &Pool,
    slug: &str,
    redirect_uri: String,
) -> Result<Option<(ResolvedProvider, ProviderKey)>, SsoError> {
    if let Some(p) = resolve_row::<SsoProvider>(tenant_pool, slug, redirect_uri.clone()).await? {
        let key = p.key(LinkSource::Tenant);
        return Ok(Some((p, key)));
    }
    // The tenant's row wins even when disabled: never the shared one.
    if has_slug::<SsoProvider>(tenant_pool, slug)
        .await
        .map_err(|e| SsoError::Config(format!("db: {e}")))?
    {
        return Ok(None);
    }
    Ok(
        resolve_row::<SharedSsoProvider>(registry_pool, slug, redirect_uri)
            .await?
            .map(|p| {
                let key = p.key(LinkSource::Shared);
                (p, key)
            }),
    )
}

/// The policy a tenant sign-in through `cfg` runs under: the provider, a
/// tenant override of a shared provider, then the tenant's settings. An
/// unreadable override or settings row refuses.
pub(crate) async fn tenant_policy(
    tenant_pool: &Pool,
    cfg: &ResolvedProvider,
    key: &ProviderKey,
    slug: &str,
    settings: &Result<TenantSettings, PolicyError>,
) -> EffectivePolicy {
    let mut policy = cfg.effective_policy();
    if key.source() == LinkSource::Shared {
        policy = policy.shared_override(SharedOverride::load(tenant_pool, slug).await);
    }
    match settings {
        Ok(s) => policy.tenant(s),
        Err(e) => policy.tighten(Err(e.clone())),
    }
}

/// Derive the absolute per-provider callback URL for this tenant from the
/// request — tenants are host-based, so the redirect_uri is per-host +
/// per-slug: `{scheme}://{host}{login_url}/sso/{slug}/callback`. Scheme
/// honors `X-Forwarded-Proto` from a trusted proxy, else `https`.
fn derive_redirect(parts: &Parts, routes: &RouteConfig, slug: &str) -> Option<String> {
    let host = crate::urls::HostAuthority::parse(parts.headers.get(header::HOST)?.to_str().ok()?)?;
    let scheme =
        crate::real_ip::trusted_forwarded(&parts.headers, &parts.extensions, "x-forwarded-proto")
            .unwrap_or("https");
    Some(format!(
        "{scheme}://{host}{}",
        callback_path(&routes.login_url, slug)
    ))
}

/// The IdP callback path for `slug` under the tenant login path.
pub(crate) fn callback_path(login_url: &str, slug: &str) -> String {
    format!("{login_url}/sso/{slug}/callback")
}

/// The login page's text for `?sso_error=`: a fixed message for a known
/// code only, and after a refusal the tenant's own access-denied note.
pub(crate) async fn login_notice(
    tenant_pool: &Pool,
    query: Option<&str>,
) -> Option<(&'static str, Option<String>)> {
    let code = query?
        .split('&')
        .find_map(|p| p.strip_prefix("sso_error="))?;
    let notice = match code {
        REFUSED => "You can't sign in with this account. Ask your administrator for access.",
        "denied" => "The sign-in was cancelled.",
        "expired" | "callback" | "handshake" => "The sign-in did not finish. Please try again.",
        "config" | "disabled" => "This sign-in option is not available right now.",
        _ => return None,
    };
    let note = if code == REFUSED {
        denied_note(tenant_pool).await
    } else {
        None
    };
    Some((notice, note))
}

/// The tenant's `access_denied_message`, if set and readable.
async fn denied_note(pool: &Pool) -> Option<String> {
    TenantSettings::load(pool).await.ok()?.access_denied_message
}

/// Where the login page sends a visitor straight away: the tenant's
/// default provider, or the only one when the password form is hidden.
/// Never with `?local=1`, after an SSO error or with a flash message, so a
/// refused sign-in does not loop.
pub(crate) fn auto_redirect(
    settings: &TenantSettings,
    providers: &[ProviderButton],
    query: Option<&str>,
    has_flash: bool,
) -> Option<String> {
    let pairs: Vec<&str> = query.unwrap_or("").split('&').collect();
    if has_flash
        || pairs
            .iter()
            .any(|p| *p == "local=1" || p.starts_with("sso_error="))
    {
        return None;
    }
    let pick = match &settings.default_provider {
        Some(slug) => providers.iter().find(|b| &b.slug == slug),
        None if !settings.shows_password_form(false) && providers.len() == 1 => providers.first(),
        None => None,
    };
    pick.map(|b| b.login_url.clone())
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
    org: &Org,
    slug: &str,
    secret: &SessionSecret,
    tenant_pool: &Pool,
    registry_pool: &Pool,
    routes: &RouteConfig,
    cookie_path: &str,
    parts: &Parts,
) -> Response {
    let scope = crate::sso::policy::throttle_scope(Some(&org.slug));
    if let Err(refused) =
        crate::sso::policy::throttle_check(&scope, &parts.extensions, &parts.headers).await
    {
        return refused;
    }
    let Some(redirect_uri) = derive_redirect(parts, routes, slug) else {
        return login_error(routes, "config");
    };
    let (cfg, key) = match resolve_by_slug(tenant_pool, registry_pool, slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(routes, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "begin resolve: {e}");
            return login_error(routes, "config");
        }
    };
    let provider = match build_provider_for(&cfg.sso, key.source()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "begin build: {e}");
            return login_error(routes, "config");
        }
    };
    let (url, flow) = provider.begin();
    let sealed = seal_flow(
        &flow,
        FlowScope::new(FlowPurpose::TenantAdmin, &org.slug, slug),
        secret.key(),
    );
    let flow_cookie = format!(
        "{SSO_FLOW_COOKIE}={sealed}; Path={cookie_path}; HttpOnly; SameSite=Lax; Max-Age=600{}",
        secure_suffix()
    );
    let mut resp = Redirect::to(&url).into_response();
    set_cookie(&mut resp, &flow_cookie);
    resp
}

/// `GET {login_url}/sso/{slug}/callback` — finish the handshake, sign in
/// the linked tenant user, mint the tenant session.
pub(super) async fn tenant_sso_callback(
    org: &Org,
    slug: &str,
    secret: &SessionSecret,
    tenant_pool: &Pool,
    registry_pool: &Pool,
    routes: &RouteConfig,
    cookie_path: &str,
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
    let Some(sealed) = crate::cookies::cookie_from_headers(&parts.headers, SSO_FLOW_COOKIE) else {
        return login_error(routes, "expired");
    };
    let flow = match open_flow(
        sealed,
        FlowScope::new(FlowPurpose::TenantAdmin, &org.slug, slug),
        secret.key(),
    ) {
        Ok(f) => f,
        Err(_) => return login_error(routes, "expired"),
    };
    let scope = crate::sso::policy::throttle_scope(Some(&org.slug));
    let attempt =
        match crate::sso::policy::throttle_begin(&scope, &parts.extensions, &parts.headers).await {
            Ok(a) => a,
            Err(refused) => return refused,
        };
    let Some(redirect_uri) = derive_redirect(parts, routes, slug) else {
        return login_error(routes, "config");
    };
    let (cfg, key) = match resolve_by_slug(tenant_pool, registry_pool, slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(routes, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "callback resolve: {e}");
            return login_error(routes, "config");
        }
    };
    let provider = match build_provider_for(&cfg.sso, key.source()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", "callback build: {e}");
            return login_error(routes, "config");
        }
    };
    let identity = match provider.complete(&flow, &code, &cb_state).await {
        Ok((u, tokens)) => Identity::from_handshake(&provider, u, &tokens).await,
        Err(e) => {
            tracing::warn!(target: "rustango::tenancy::sso", "handshake: {e}");
            attempt.failed().await;
            return login_error(routes, "handshake");
        }
    };
    let normalized = &identity.profile;
    let meta = || meta_from_parts(&parts.extensions, &parts.headers, Some(parts.uri.path()));
    let settings = TenantSettings::load(tenant_pool).await;
    let policy = tenant_policy(tenant_pool, &cfg, &key, slug, &settings).await;
    let request = AdmitRequest::new(tenant_pool, Surface::TenantAdmin, &key, &policy, &identity)
        .provider(slug);
    let uid = match admit(request, &member_auth::TenantAccounts(tenant_pool)).await {
        Ok(a) => a.user_id,
        Err(Denied::Refused(r)) => {
            let ctx = RefusalContext {
                pool: tenant_pool,
                surface: Surface::TenantAdmin,
                signal_source: SIGNAL_SOURCE,
                provider: slug,
                key: &key,
                identity: &identity,
                tenant: Some(org.slug.clone()),
                extensions: &parts.extensions,
                headers: &parts.headers,
                path: parts.uri.path(),
            };
            refuse(&ctx, &r).await;
            attempt.failed().await;
            return login_error(routes, REFUSED);
        }
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::sso", tenant = %org.slug, slug, "sso sign-in failed: {e}");
            return login_error(routes, REFUSED);
        }
    };
    let Some(user) = member_auth::tenant_user(tenant_pool, uid).await else {
        return login_error(routes, REFUSED);
    };
    if !user.active {
        signal_refused(
            SIGNAL_SOURCE,
            normalized,
            AuthFailureReason::Inactive,
            meta(),
            Some(org.slug.clone()),
        )
        .await;
        return login_error(routes, REFUSED);
    }

    // Mint the tenant session — same shape as a password login.
    attempt.succeeded().await;
    let route_ttl = i64::try_from(routes.tenant_session_ttl.as_secs())
        .unwrap_or(tenant_console::SESSION_TTL_SECS);
    let ttl = policy.session_ttl(route_ttl, user.is_superuser);
    let mut payload = TenantSessionPayload::new(
        uid,
        &org.slug,
        ttl,
        super::session::PasswordFingerprint::of(secret, &user.password_hash),
    );
    payload.iat = crate::session::issued_at(user.sessions_revoked_at);
    let cookie_value = tenant_console::encode(secret, &payload);
    let session_cookie = format!(
        "{}={cookie_value}; Path={cookie_path}; HttpOnly; SameSite=Lax; Max-Age={ttl}{}",
        tenant_console::COOKIE_NAME,
        secure_suffix()
    );
    let clear_flow = format!(
        "{SSO_FLOW_COOKIE}=; Path={cookie_path}; HttpOnly; SameSite=Lax; Max-Age=0{}",
        secure_suffix()
    );
    let mut resp = Redirect::to(routes.admin_url.as_str()).into_response();
    set_cookie(&mut resp, &session_cookie);
    set_cookie(&mut resp, &clear_flow);
    send_user_logged_in(UserLoggedInContext {
        source: SIGNAL_SOURCE,
        user_id: uid,
        username: user.username.clone(),
        is_superuser: user.is_superuser,
        request: meta(),
        tenant: Some(org.slug.clone()),
    })
    .await;
    resp
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

    /// The `redirect_uri` never takes userinfo from the Host (#2173).
    #[test]
    fn derive_redirect_refuses_a_host_with_userinfo() {
        let routes = super::RouteConfig::default();
        let parts = |host: &str| {
            axum::http::Request::builder()
                .header("host", host)
                .body(())
                .unwrap()
                .into_parts()
                .0
        };
        assert!(super::derive_redirect(&parts("good.com:1@evil.com:2"), &routes, "g").is_none());
        assert!(super::derive_redirect(&parts("good.com@evil.com"), &routes, "g").is_none());
        let ok = super::derive_redirect(&parts("good.com:8443"), &routes, "g").unwrap();
        assert!(ok.starts_with("https://good.com:8443/"), "{ok}");
    }
}
