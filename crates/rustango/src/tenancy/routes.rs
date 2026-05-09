//! [`RouteConfig`] — apex / tenant URL prefixes (#74, v0.28.0).
//!
//! Pre-0.28, every framework-internal URL was hardcoded:
//! `/__login`, `/__logout`, `/__admin`, `/__audit`, `/__static__`,
//! `/__brand__`. The `__` prefix was chosen to avoid colliding with
//! user routes, but apps shipping to production want friendly URLs
//! (`/login`, `/admin`) that match user expectations.
//!
//! **v0.29 default flip (#85):** [`RouteConfig::default`] now
//! returns the friendly preset (`/login`, `/admin`, `/audit`, …).
//! Projects that need to keep the v0.28 `__`-prefixed paths must
//! opt in via `RouteConfig::legacy()` or set the relevant fields
//! manually. Migration: existing v0.28 deployments calling
//! `Default::default()` will see their admin/login URLs change
//! shape — bookmarks and external integrations need updating.
//!
//! `RouteConfig` exposes those prefixes so any
//! `Server::Builder` / `TenantAdminBuilder` /
//! `operator_console::router_*` consumer can override them.
//!
//! ## Wiring
//!
//! ```ignore
//! use rustango::tenancy::RouteConfig;
//!
//! let routes = RouteConfig {
//!     login_url:  "/login".to_owned(),
//!     logout_url: "/logout".to_owned(),
//!     admin_url:  "/admin".to_owned(),
//!     ..Default::default()
//! };
//!
//! // Tenancy server builder picks it up via `routes(...)` setter.
//! ```
//!
//! ## Reserved namespace caveat
//!
//! When apps move away from the `__` prefix, user routes like
//! `/admin/settings` could collide with the admin mount. The
//! framework doesn't enforce non-collision — pick a prefix nothing
//! else in your app uses.

use std::time::Duration;

/// Apex / tenant URL prefixes. Every framework-internal URL is
/// derived from this struct (login, logout, admin, audit, static
/// asset paths, brand asset paths, plus the HTTP Basic auth
/// realm string). Defaults match the pre-0.28 hardcoded values
/// so existing apps don't break on upgrade.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    /// URL of the login form / submit endpoint, on both the
    /// tenant admin and the operator console. Default
    /// `/login` (since v0.29; was `/__login` pre-flip).
    pub login_url: String,
    /// URL of the logout endpoint (POST) on both consoles.
    /// Default `/logout` (since v0.29; was `/__logout` pre-flip).
    pub logout_url: String,
    /// URL prefix for the per-tenant admin Router. Same value
    /// the `admin_prefix` template variable carries (#59). The
    /// tenant admin mounts under `<admin_url>/{*rest}`. Must
    /// match between the tenant admin and operator console
    /// when impersonation is enabled, since the operator
    /// console builds the tenant-admin redirect URL from this
    /// value. Default `/admin` (since v0.29; was `/__admin`).
    pub admin_url: String,
    /// URL of the audit-log view inside the admin. Joined with
    /// `admin_url` in templates as
    /// `{{ admin_prefix }}{{ audit_url }}`. Default `/audit`
    /// (since v0.29; was `/__audit`).
    pub audit_url: String,
    /// URL prefix for embedded static assets (rustango.png,
    /// theme tokens) served for the operator console and
    /// per-tenant admin. Default `/_static` (since v0.29; was
    /// `/__static__`).
    pub static_url: String,
    /// URL prefix for tenant brand asset serving fallback —
    /// `<brand_url>/{slug}/{filename}` resolves to the org's
    /// uploaded logo / favicon when the configured brand
    /// `Storage` doesn't expose direct URLs. Default
    /// `/_brand` (since v0.29; was `/__brand__`).
    pub brand_url: String,
    /// URL of the self-serve change-password page on the tenant
    /// admin (#77, v0.28.2). GET renders a form (current password
    /// + new password + confirm); POST verifies the current
    /// password and updates `rustango_users.password_hash`. The
    /// route lives outside [`Self::admin_url`] so it stays a
    /// distinct namespace from per-table admin routes. Default
    /// `/change-password` (since v0.29; was `/__change-password`).
    pub change_password_url: String,
    /// URL on the tenant admin where the operator console's
    /// impersonation flow lands the browser to redeem a signed
    /// handoff token (#88). Solves the Chromium-localhost
    /// public-suffix-list cookie problem: the operator console
    /// can't set a `Domain=.localhost` cookie that subdomains
    /// receive, so it instead 302s to this URL with a `?token=`
    /// query parameter, and the tenant admin sets a host-scoped
    /// cookie (no `Domain=`) which Chromium accepts. Default
    /// `/_impersonation_handoff` (since v0.29).
    pub impersonation_handoff_url: String,
    /// HTTP Basic auth realm. Used by the legacy
    /// `protect_with_basic_auth` admin gate (pre-tenancy
    /// projects); displayed by the browser's auth dialog.
    /// Default `"Rustango Admin"`.
    pub basic_auth_realm: String,
    /// Tenant-admin session lifetime. Default 7 days. Apps with
    /// tighter compliance requirements lower this; ones with
    /// a "remember me" UX raise it.
    pub tenant_session_ttl: Duration,
    /// Operator-console session lifetime. Default 7 days.
    pub operator_session_ttl: Duration,
    /// Operator-as-superuser impersonation cookie lifetime
    /// (#78). Default 1 hour. Short by design — long enough
    /// for a debugging session, short enough that an idle
    /// operator gets dropped before they can forget they're
    /// impersonating.
    pub impersonation_ttl: Duration,
}

/// `Default` returns the friendly preset (Django-style) since
/// v0.29 (#85). Projects that need the v0.28 `__`-prefixed URLs
/// must opt in via [`RouteConfig::legacy`].
impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            login_url: "/login".to_owned(),
            logout_url: "/logout".to_owned(),
            admin_url: "/admin".to_owned(),
            audit_url: "/audit".to_owned(),
            static_url: "/_static".to_owned(),
            brand_url: "/_brand".to_owned(),
            change_password_url: "/change-password".to_owned(),
            impersonation_handoff_url: "/_impersonation_handoff".to_owned(),
            basic_auth_realm: "Rustango Admin".to_owned(),
            tenant_session_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            operator_session_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            impersonation_ttl: Duration::from_secs(60 * 60),
        }
    }
}

impl RouteConfig {
    /// Alias for [`RouteConfig::default`] since v0.29 — friendly
    /// is now the default. Kept for source-compatibility with code
    /// written against v0.28 that explicitly opted into
    /// `friendly()`.
    #[must_use]
    pub fn friendly() -> Self {
        Self::default()
    }

    /// Pre-v0.29 default — `/__login`, `/__admin`, `/__audit`,
    /// `/__static__`, `/__brand__`, `/__change-password`. Use
    /// when:
    /// - your app has user routes at `/admin` or `/login` that
    ///   would collide with the friendly defaults, OR
    /// - you're upgrading a v0.28 deployment and want to preserve
    ///   bookmarks / external integrations / SSO callback URLs
    ///   that point at the `__`-prefixed paths.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .tenancy()
    ///     .routes(RouteConfig::legacy())
    ///     .api(urls::api())
    ///     .run().await
    /// ```
    #[must_use]
    pub fn legacy() -> Self {
        Self {
            login_url: "/__login".to_owned(),
            logout_url: "/__logout".to_owned(),
            admin_url: "/__admin".to_owned(),
            audit_url: "/__audit".to_owned(),
            static_url: "/__static__".to_owned(),
            brand_url: "/__brand__".to_owned(),
            change_password_url: "/__change-password".to_owned(),
            // Note: keep the underscore-prefixed handoff URL even on
            // the legacy preset — pre-#88 deployments didn't have
            // this surface at all, so any path is non-breaking; the
            // single underscore matches the rest of `legacy()` style.
            impersonation_handoff_url: "/__impersonation_handoff".to_owned(),
            ..Default::default()
        }
    }

    /// Joined path for the audit view: `<admin_url><audit_url>`.
    /// Useful for emit sites that need the full external URL
    /// without involving Tera.
    #[must_use]
    pub fn audit_full_url(&self) -> String {
        format!("{}{}", self.admin_url, self.audit_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Since v0.29 (#85), `Default::default()` returns the
    /// friendly preset. Apps that haven't opted into `.routes(...)`
    /// see `/login`, `/admin`, `/audit`, etc.
    #[test]
    fn defaults_now_match_friendly() {
        let r = RouteConfig::default();
        assert_eq!(r.login_url, "/login");
        assert_eq!(r.logout_url, "/logout");
        assert_eq!(r.admin_url, "/admin");
        assert_eq!(r.audit_url, "/audit");
        assert_eq!(r.static_url, "/_static");
        assert_eq!(r.brand_url, "/_brand");
        assert_eq!(r.change_password_url, "/change-password");
        assert_eq!(r.impersonation_handoff_url, "/_impersonation_handoff");
        assert_eq!(r.basic_auth_realm, "Rustango Admin");
    }

    /// `friendly()` is now an alias for `default()` — same
    /// shape, kept for source-compat with v0.28 code that
    /// explicitly opted in.
    #[test]
    fn friendly_is_alias_for_default() {
        let f = RouteConfig::friendly();
        let d = RouteConfig::default();
        assert_eq!(f.login_url, d.login_url);
        assert_eq!(f.admin_url, d.admin_url);
        assert_eq!(f.audit_url, d.audit_url);
        assert_eq!(f.static_url, d.static_url);
        assert_eq!(f.brand_url, d.brand_url);
        assert_eq!(f.change_password_url, d.change_password_url);
    }

    /// `legacy()` returns the pre-v0.29 `__`-prefixed shape.
    /// Use when upgrading deployments that need to preserve
    /// bookmarks or external integrations.
    #[test]
    fn legacy_preset_keeps_underscores() {
        let r = RouteConfig::legacy();
        assert_eq!(r.login_url, "/__login");
        assert_eq!(r.logout_url, "/__logout");
        assert_eq!(r.admin_url, "/__admin");
        assert_eq!(r.audit_url, "/__audit");
        assert_eq!(r.static_url, "/__static__");
        assert_eq!(r.brand_url, "/__brand__");
        assert_eq!(r.change_password_url, "/__change-password");
        assert_eq!(r.impersonation_handoff_url, "/__impersonation_handoff");
    }

    #[test]
    fn audit_full_url_joins_prefixes() {
        assert_eq!(RouteConfig::default().audit_full_url(), "/admin/audit");
        assert_eq!(RouteConfig::legacy().audit_full_url(), "/__admin/__audit");
    }

    #[test]
    fn ttls_have_sensible_defaults() {
        let r = RouteConfig::default();
        assert_eq!(r.tenant_session_ttl.as_secs(), 7 * 24 * 60 * 60);
        assert_eq!(r.operator_session_ttl.as_secs(), 7 * 24 * 60 * 60);
        assert_eq!(r.impersonation_ttl.as_secs(), 60 * 60);
    }
}
