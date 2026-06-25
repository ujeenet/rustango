//! Django parity — `Settings.security` deploy-audit warnings for
//! `ALLOWED_HOSTS`, `CSRF_TRUSTED_ORIGINS`, `SECURE_SSL_REDIRECT`,
//! `SECURE_PROXY_SSL_HEADER`. `manage check --deploy` on the prod
//! tier walks the new fields + emits warnings when ops left them at
//! dev-time defaults.

#![cfg(all(feature = "config"))]

use rustango::config::Settings;
use rustango::migrate::manage::{settings_audit_check, DeployAuditFindings};

fn audit_prod(tweak: impl FnOnce(&mut Settings)) -> DeployAuditFindings {
    let mut s = Settings::default();
    tweak(&mut s);
    let mut out = DeployAuditFindings::default();
    settings_audit_check("prod", &s, &mut out);
    out
}

#[test]
fn empty_allowed_hosts_in_prod_warns() {
    let out = audit_prod(|_| {});
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("allowed_hosts = []")),
        "expected ALLOWED_HOSTS warning; got warnings: {:?}",
        out.warnings
    );
}

#[test]
fn populated_allowed_hosts_in_prod_is_silent() {
    let out = audit_prod(|s| {
        s.security.allowed_hosts = vec!["example.com".into(), ".example.com".into()];
    });
    assert!(
        out.warnings
            .iter()
            .all(|w| !w.contains("allowed_hosts = []")),
        "expected NO ALLOWED_HOSTS warning when populated; got: {:?}",
        out.warnings
    );
}

#[test]
fn empty_csrf_trusted_origins_emits_info_not_warning() {
    let out = audit_prod(|_| {});
    // Promoted to info — back-compat default.
    assert!(
        out.info.iter().any(|m| m.contains("csrf_trusted_origins")),
        "expected info about csrf_trusted_origins; got: {:?}",
        out.info
    );
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.contains("csrf_trusted_origins")),
        "csrf_trusted_origins should not be a warning by default; got: {:?}",
        out.warnings
    );
}

#[test]
fn missing_ssl_redirect_in_prod_emits_info() {
    let out = audit_prod(|_| {});
    assert!(
        out.info.iter().any(|m| m.contains("secure_ssl_redirect")),
        "expected info about secure_ssl_redirect; got: {:?}",
        out.info
    );
}

#[test]
fn explicit_ssl_redirect_true_clears_the_info() {
    let out = audit_prod(|s| {
        s.security.secure_ssl_redirect = Some(true);
    });
    assert!(
        !out.info.iter().any(|m| m.contains("secure_ssl_redirect")),
        "expected no info when SSL redirect is on; got: {:?}",
        out.info
    );
}

#[test]
fn malformed_proxy_ssl_header_warns() {
    let out = audit_prod(|s| {
        // Only one entry — expected two.
        s.security.secure_proxy_ssl_header = vec!["X-Forwarded-Proto".into()];
    });
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("secure_proxy_ssl_header")),
        "expected warning about malformed proxy_ssl_header; got: {:?}",
        out.warnings
    );
}

#[test]
fn well_formed_proxy_ssl_header_is_silent() {
    let out = audit_prod(|s| {
        s.security.secure_proxy_ssl_header = vec!["X-Forwarded-Proto".into(), "https".into()];
    });
    assert!(
        out.warnings
            .iter()
            .all(|w| !w.contains("secure_proxy_ssl_header")),
        "expected no warning when pair is well-formed; got: {:?}",
        out.warnings
    );
}

// #1099 — the [mcp] deploy rule fires only when the MCP server is compiled in.
#[cfg(feature = "mcp")]
#[test]
fn mcp_prod_audit_flags_empty_origins_and_no_rate_limit() {
    let out = audit_prod(|_| {});
    assert!(
        out.info.iter().any(|m| m.contains("[mcp] allowed_origins")),
        "expected [mcp] allowed_origins info; got: {:?}",
        out.info
    );
    assert!(
        out.info
            .iter()
            .any(|m| m.contains("[mcp] rate_limit_per_minute")),
        "expected [mcp] rate-limit info; got: {:?}",
        out.info
    );
}

#[cfg(feature = "mcp")]
#[test]
fn mcp_prod_audit_silent_when_configured() {
    let out = audit_prod(|s| {
        s.mcp.allowed_origins = vec!["https://app.example".into()];
        s.mcp.rate_limit_per_minute = Some(120);
    });
    assert!(
        !out.info.iter().any(|m| m.contains("[mcp]")),
        "configured [mcp] should be silent; got: {:?}",
        out.info
    );
}

#[test]
fn dev_tier_does_not_run_security_audit() {
    let mut s = Settings::default();
    // Leave allowed_hosts empty — would be a prod warning.
    let mut out = DeployAuditFindings::default();
    settings_audit_check("dev", &s, &mut out);
    // Dev tier returns early — no security-section warnings fire.
    assert!(
        out.warnings.is_empty() && out.info.is_empty(),
        "dev tier should skip the prod-only audit; got warnings={:?} info={:?}",
        out.warnings,
        out.info
    );
    // Avoid unused-mut on s if the audit ever starts caring about
    // dev tier — touch the field to keep the trace honest.
    s.security.allowed_hosts.clear();
}
