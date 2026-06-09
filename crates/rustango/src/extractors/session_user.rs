//! `SessionUser` and `SessionOperator` extractors — read the current
//! user / operator from the browser session cookie.
//!
//! Both are infallible (`Rejection = Infallible`) — they return `None`
//! for anonymous requests rather than rejecting, so public routes can
//! still use them without forcing every visitor to be logged in.
//!
//! # Usage
//!
//! ```ignore
//! use rustango::extractors::{SessionUser, SessionOperator, Tenant};
//!
//! // Tenant route — identifies the browser-session user:
//! pub async fn profile(
//!     mut t: Tenant,
//!     SessionUser(user): SessionUser,
//! ) -> impl IntoResponse {
//!     match user {
//!         Some(u) => format!("hello, {}", u.username).into_response(),
//!         None    => StatusCode::UNAUTHORIZED.into_response(),
//!     }
//! }
//!
//! // Operator console route — identifies the logged-in operator:
//! pub async fn operator_dashboard(
//!     SessionOperator(op): SessionOperator,
//! ) -> impl IntoResponse {
//!     match op {
//!         Some(o) => format!("operator: {}", o.username).into_response(),
//!         None    => StatusCode::UNAUTHORIZED.into_response(),
//!     }
//! }
//! ```

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::tenancy::auth::{Operator, User};
use crate::tenancy::{operator_console, tenant_console, OrgResolver as _};

use super::TenantContext;

// ------------------------------------------------------------------ SessionUser

/// Reads the `rustango_tenant_session` browser cookie and returns the
/// corresponding [`User`] row, or `None` for anonymous / expired sessions.
///
/// Requires the [`crate::server::Builder`] stack — the extractor reads
/// the session secret and resolver from the `Arc<TenantContext>` extension
/// that `Builder::serve` injects. Returns `None` (never rejects) so it
/// composes safely with public routes.
///
/// The resolved org's slug is used to validate the tenant binding in the
/// cookie — a cookie minted for `acme` will never authenticate on `globex`.
pub struct SessionUser(pub Option<User>);

impl<S: Send + Sync> FromRequestParts<S> for SessionUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(ctx) = parts.extensions.get::<Arc<TenantContext>>().cloned() else {
            return Ok(SessionUser(None));
        };

        // Resolve the tenant — needed for the slug binding check and the
        // pool to look up the user row.
        let org = match ctx
            .resolver
            .resolve(parts, &ctx.pools.registry_pool())
            .await
        {
            Ok(Some(o)) => o,
            _ => return Ok(SessionUser(None)),
        };

        let cookie_value = match extract_cookie(parts, tenant_console::COOKIE_NAME) {
            Some(v) => v,
            None => return Ok(SessionUser(None)),
        };

        let payload = match tenant_console::decode(&ctx.session_secret, &org.slug, &cookie_value) {
            Ok(p) => p,
            Err(_) => return Ok(SessionUser(None)),
        };

        // v0.41 (#317) — route through the tri-dialect Pool enum
        // instead of acquiring a backend-specific TenantConn. This
        // lets `SessionUser` resolve on sqlite + mysql tenancy builds
        // the same way `SessionOperator` already does (via
        // `fetch_pool` on the registry pool below).
        let pool = match ctx.pools.scoped_pool_dyn(&org).await {
            Ok(p) => p,
            Err(_) => return Ok(SessionUser(None)),
        };

        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        let users = User::objects()
            .where_(User::id.eq(payload.uid))
            .fetch_pool(&pool)
            .await
            .unwrap_or_default();

        let user = users.into_iter().next().filter(|u| u.active);
        // Audit P4 — reject a session minted before the user's last
        // password change (parity with the tenant-admin gate at
        // `tenancy::admin::validate_session`). `password_changed_at IS
        // NULL` (never rotated since the column existed) stays valid.
        let user = user.filter(|u| match u.password_changed_at {
            Some(changed) => payload.iat >= changed.timestamp(),
            None => true,
        });
        Ok(SessionUser(user))
    }
}

// ------------------------------------------------------------------ SessionOperator

/// Reads the `rustango_op_session` browser cookie and returns the
/// corresponding [`Operator`] row, or `None` for anonymous / expired sessions.
///
/// Uses the `operator_secret` stored in [`TenantContext`] by
/// [`crate::server::Builder`]. Returns `None` (never rejects).
pub struct SessionOperator(pub Option<Operator>);

impl<S: Send + Sync> FromRequestParts<S> for SessionOperator {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(ctx) = parts.extensions.get::<Arc<TenantContext>>().cloned() else {
            return Ok(SessionOperator(None));
        };

        let cookie_value = match extract_cookie(parts, operator_console::session::COOKIE_NAME) {
            Some(v) => v,
            None => return Ok(SessionOperator(None)),
        };

        let payload = match operator_console::session::decode(&ctx.operator_secret, &cookie_value) {
            Ok(p) => p,
            Err(_) => return Ok(SessionOperator(None)),
        };

        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        let ops = Operator::objects()
            .where_(Operator::id.eq(payload.oid))
            .fetch_pool(&ctx.pools.registry_pool())
            .await
            .unwrap_or_default();

        let op = ops.into_iter().next().filter(|o| o.active);
        Ok(SessionOperator(op))
    }
}

// ------------------------------------------------------------------ helpers

fn extract_cookie<'a>(parts: &'a Parts, name: &str) -> Option<String> {
    let header = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(name) {
            if val.starts_with('=') {
                return Some(val[1..].to_owned());
            }
        }
    }
    None
}
