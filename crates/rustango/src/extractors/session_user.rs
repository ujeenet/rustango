//! The `SessionUser` and `SessionOperator` extractors read the
//! current user or operator from the browser's session cookie.
//!
//! Neither can fail. An anonymous request gives `None` rather than a
//! rejection, so a public route can use them without forcing every
//! visitor to log in.
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

/// Reads the `rustango_tenant_session` cookie and returns the
/// matching [`User`], or `None` when the session is missing or has
/// expired. It never rejects.
///
/// It needs the [`crate::server::Builder`] stack, because it takes
/// the session secret and the resolver from the `TenantContext`
/// extension that `Builder::serve` adds.
///
/// The cookie is bound to the tenant slug, so a cookie minted for
/// `acme` never authenticates on `globex`.
pub struct SessionUser(pub Option<User>);

impl<S: Send + Sync> FromRequestParts<S> for SessionUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(ctx) = parts.extensions.get::<Arc<TenantContext>>().cloned() else {
            return Ok(SessionUser(None));
        };

        // Resolve the tenant: needed for the slug check, and for the
        // pool that holds the user row.
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

        // Go through the `Pool` enum rather than a backend-specific
        // `TenantConn`, so this works on SQLite and MySQL too.
        let pool = match ctx.pools.scoped_pool_dyn(&org).await {
            Ok(p) => p,
            Err(_) => return Ok(SessionUser(None)),
        };

        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        let users = User::objects()
            .where_(User::id.eq(payload.uid))
            .fetch(&pool)
            .await
            .unwrap_or_default();

        let user = users.into_iter().next().filter(|u| u.active);
        // Reject a session minted before the user's last password
        // change, as `tenancy::admin::validate_session` does. A null
        // `password_changed_at` means it never changed, so it stays
        // valid.
        let user = user.filter(|u| match u.password_changed_at {
            Some(changed) => payload.iat >= changed.timestamp(),
            None => true,
        });
        Ok(SessionUser(user))
    }
}

// ------------------------------------------------------------------ SessionOperator

/// Reads the `rustango_op_session` cookie and returns the matching
/// [`Operator`], or `None` when the session is missing or has
/// expired. It never rejects.
///
/// It uses the `operator_secret` that [`crate::server::Builder`] put
/// in [`TenantContext`].
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
            .fetch(&ctx.pools.registry_pool())
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
