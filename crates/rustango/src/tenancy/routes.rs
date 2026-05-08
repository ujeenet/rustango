//! [`RouteConfig`] — apex / tenant URL prefixes (#74, v0.28.0).
//!
//! Pre-0.28, every framework-internal URL was hardcoded:
//! `/__login`, `/__logout`, `/__admin`, `/__audit`, `/__static__`,
//! `/__brand__`. The `__` prefix was chosen to avoid colliding with
//! user routes, but apps shipping to production want friendly URLs
//! (`/login`, `/admin`) that match user expectations.
//!
//! `RouteConfig` exposes those prefixes so any
//! `Server::Builder` / `TenantAdminBuilder` /
//! `operator_console::router_*` consumer can override them. Defaults
//! preserve every pre-0.28 path so upgrading is a no-op until apps
//! explicitly change `routes`.
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
    /// `/__login`.
    pub login_url: String,
    /// URL of the logout endpoint (POST) on both consoles.
    /// Default `/__logout`.
    pub logout_url: String,
    /// URL prefix for the per-tenant admin Router. Same value
    /// the `admin_prefix` template variable carries (#59). The
    /// tenant admin mounts under `<admin_url>/{*rest}`. Must
    /// match between the tenant admin and operator console
    /// when impersonation is enabled, since the operator
    /// console builds the tenant-admin redirect URL from this
    /// value. Default `/__admin`.
    pub admin_url: String,
    /// URL of the audit-log view inside the admin. Joined with
    /// `admin_url` in templates as
    /// `{{ admin_prefix }}{{ audit_url }}`. Default `/__audit`.
    pub audit_url: String,
    /// URL prefix for embedded static assets (rustango.png,
    /// theme tokens served from `<scheme>://<apex>/__static__/...`
    /// for the operator console and per-tenant admin. Default
    /// `/__static__`.
    pub static_url: String,
    /// URL prefix for tenant brand asset serving fallback —
    /// `<brand_url>/{slug}/{filename}` resolves to the org's
    /// uploaded logo / favicon when the configured brand
    /// `Storage` doesn't expose direct URLs. Default
    /// `/__brand__`.
    pub brand_url: String,
    /// URL of the self-serve change-password page on the tenant
    /// admin (#77, v0.28.2). GET renders a form (current password
    /// + new password + confirm); POST verifies the current
    /// password and updates `rustango_users.password_hash`. The
    /// route lives outside [`Self::admin_url`] so it stays a
    /// distinct namespace from per-table admin routes. Default
    /// `/__change-password`.
    pub change_password_url: String,
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

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            login_url: "/__login".to_owned(),
            logout_url: "/__logout".to_owned(),
            admin_url: "/__admin".to_owned(),
            audit_url: "/__audit".to_owned(),
            static_url: "/__static__".to_owned(),
            brand_url: "/__brand__".to_owned(),
            change_password_url: "/__change-password".to_owned(),
            basic_auth_realm: "Rustango Admin".to_owned(),
            tenant_session_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            operator_session_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            impersonation_ttl: Duration::from_secs(60 * 60),
        }
    }
}

impl RouteConfig {
    /// Construct from a Django-style "production-friendly" preset:
    /// `/login`, `/logout`, `/admin`, etc. without the `__`
    /// prefix. Useful when an app has reserved its namespace
    /// well enough that `/admin` doesn't collide with user
    /// routes.
    #[must_use]
    pub fn friendly() -> Self {
        Self {
            login_url: "/login".to_owned(),
            logout_url: "/logout".to_owned(),
            admin_url: "/admin".to_owned(),
            audit_url: "/audit".to_owned(),
            static_url: "/_static".to_owned(),
            brand_url: "/_brand".to_owned(),
            change_password_url: "/change-password".to_owned(),
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

    #[test]
    fn defaults_match_legacy_prefixes() {
        // v0.27 → v0.28 compatibility guard. Apps that don't
        // override `routes` must see identical URL behavior.
        let r = RouteConfig::default();
        assert_eq!(r.login_url, "/__login");
        assert_eq!(r.logout_url, "/__logout");
        assert_eq!(r.admin_url, "/__admin");
        assert_eq!(r.audit_url, "/__audit");
        assert_eq!(r.static_url, "/__static__");
        assert_eq!(r.brand_url, "/__brand__");
        assert_eq!(r.change_password_url, "/__change-password");
        assert_eq!(r.basic_auth_realm, "Rustango Admin");
    }

    #[test]
    fn friendly_preset_drops_underscores() {
        let r = RouteConfig::friendly();
        assert_eq!(r.login_url, "/login");
        assert_eq!(r.admin_url, "/admin");
        assert_eq!(r.audit_url, "/audit");
        assert_eq!(r.change_password_url, "/change-password");
        // Realm stays the same (it's a display string, not a path).
        assert_eq!(r.basic_auth_realm, "Rustango Admin");
    }

    #[test]
    fn audit_full_url_joins_prefixes() {
        assert_eq!(RouteConfig::default().audit_full_url(), "/__admin/__audit");
        assert_eq!(RouteConfig::friendly().audit_full_url(), "/admin/audit");
    }

    #[test]
    fn ttls_have_sensible_defaults() {
        let r = RouteConfig::default();
        assert_eq!(r.tenant_session_ttl.as_secs(), 7 * 24 * 60 * 60);
        assert_eq!(r.operator_session_ttl.as_secs(), 7 * 24 * 60 * 60);
        assert_eq!(r.impersonation_ttl.as_secs(), 60 * 60);
    }
}
