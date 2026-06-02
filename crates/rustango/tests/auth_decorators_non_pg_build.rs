//! Issue #560 — `auth_decorators::*` (login_required / permission_required /
//! superuser_required / user_passes_test*) previously gated each
//! public function on `#[cfg(all(feature = "tenancy", feature = "postgres"))]`,
//! so a SQLite-only or MySQL-only build couldn't gate any route. This
//! test exercises the public surface on a non-PG `--features sqlite,
//! tenancy,template_views` build to lock the compile-gap closed.
//!
//! Runtime behavior (the actual `next.run(req)` dispatch) is covered
//! by the framework's PG e2e suite; here we just nail down the API.

#![cfg(all(feature = "sqlite", feature = "tenancy", not(feature = "postgres")))]

use rustango::auth_decorators::{
    login_required, permission_required, permission_required_or_403, superuser_required,
    user_passes_test, user_passes_test_or_403, LoginRequiredConfig,
};

#[test]
fn login_required_constructs_on_sqlite_only_build() {
    // The compile-gap fix means this line links on a no-PG build.
    let _layer = login_required("/login");
}

#[test]
fn permission_required_constructs_on_sqlite_only_build() {
    let _layer = permission_required("/login", "auth.access_admin");
}

#[test]
fn permission_required_or_403_constructs_on_sqlite_only_build() {
    let _layer = permission_required_or_403("auth.access_admin");
}

#[test]
fn superuser_required_constructs_on_sqlite_only_build() {
    let _layer = superuser_required("/login");
}

#[test]
fn user_passes_test_constructs_on_sqlite_only_build() {
    let _layer = user_passes_test("/login", |u| u.is_superuser);
}

#[test]
fn user_passes_test_or_403_constructs_on_sqlite_only_build() {
    let _layer = user_passes_test_or_403(|u| u.is_superuser);
}

#[test]
fn login_required_config_uses_default_redirect_field() {
    let cfg = LoginRequiredConfig::default();
    assert_eq!(cfg.login_url, "/login");
    assert_eq!(cfg.redirect_field, "next");
}
