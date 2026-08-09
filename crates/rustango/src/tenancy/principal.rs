//! Who a request is acting as — one type, whatever authenticated it.
//!
//! A request can arrive carrying a session cookie, a Bearer access token, or an
//! MCP agent token, and each of those middlewares has historically left behind
//! a different extension: [`AuthenticatedUser`], `CurrentMember`, `McpAgent`.
//! Anything downstream that needs "which user is this" — a queryset scoped to
//! its owner, an audit line, a permission check — then has to know which auth
//! path ran, and gets it wrong for the paths nobody tested.
//!
//! [`Principal`] is that answer, resolved once from whatever is present. It is
//! deliberately *not* another auth mechanism: it authenticates nothing, mints
//! nothing, and reads only what a verifying middleware already proved.
//!
//! ```no_run
//! use rustango::tenancy::Principal;
//!
//! async fn handler(principal: Principal) -> String {
//!     format!("hello, user {}", principal.user_id)
//! }
//! ```

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::middleware::AuthenticatedUser;

/// How the request proved who it is. Kept because the answer sometimes
/// matters: an agent token acts *for* a user but is not that user sitting at a
/// browser, and an audit trail that cannot tell them apart is worth less.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalKind {
    /// A signed-in user — session cookie or a Bearer access token.
    User,
    /// A machine token acting on behalf of the user who minted it.
    Agent,
}

/// The identity behind a request.
///
/// Populated by whichever middleware verified the request; see
/// [`Principal::from_parts`] for the resolution order. Absence means
/// unauthenticated — never "assume anonymous is fine".
#[derive(Clone, Debug)]
pub struct Principal {
    /// `rustango_users.id`. For an agent token this is the **owner**, so work
    /// done by a key scopes to the person who created it.
    pub user_id: i64,
    /// Authoritative only as of whenever the middleware last read the row.
    pub is_superuser: bool,
    /// Tenant slug the credential is pinned to, when it carries one. A token
    /// without this cannot be checked against the resolved tenant.
    pub tenant: Option<String>,
    pub kind: PrincipalKind,
    /// `rustango_agents.id` when [`PrincipalKind::Agent`].
    pub agent_id: Option<i64>,
}

impl Principal {
    /// A signed-in user.
    #[must_use]
    pub fn user(user_id: i64, is_superuser: bool, tenant: Option<String>) -> Self {
        Self {
            user_id,
            is_superuser,
            tenant,
            kind: PrincipalKind::User,
            agent_id: None,
        }
    }

    /// A signed-in superuser. Convenience for tests and for middleware that
    /// has already read the flag.
    #[must_use]
    pub fn admin(user_id: i64) -> Self {
        Self {
            is_superuser: true,
            ..Self::user(user_id, true, None)
        }
    }

    /// Resolve from whatever a verifying middleware left in the extensions.
    ///
    /// Order, most specific first:
    /// 1. an explicit [`Principal`] — an app that resolved its own wins;
    /// 2. [`AuthenticatedUser`] — `require_auth`, the Bearer middleware, any
    ///    auth backend;
    /// 3. an `McpAgent` **that owns a user** — the agent acts as that user. A
    ///    standalone machine agent has no user to act as and yields `None`,
    ///    which is what keeps an ownership filter from matching everyone's rows.
    ///
    /// Returns `None` when the request is unauthenticated. This function
    /// verifies nothing on its own — a forged extension would be trusted, so
    /// nothing may insert one without checking a credential first.
    #[must_use]
    pub fn from_parts(parts: &Parts) -> Option<Self> {
        if let Some(p) = parts.extensions.get::<Self>() {
            return Some(p.clone());
        }
        if let Some(u) = parts.extensions.get::<AuthenticatedUser>() {
            return Some(Self {
                user_id: u.id,
                is_superuser: u.is_superuser,
                tenant: parts
                    .extensions
                    .get::<TenantSlug>()
                    .map(|TenantSlug(s)| s.clone()),
                kind: PrincipalKind::User,
                agent_id: None,
            });
        }
        #[cfg(feature = "mcp")]
        if let Some(agent) = parts.extensions.get::<crate::mcp::auth::McpAgent>() {
            return agent.user_id.map(|uid| Self {
                user_id: uid,
                // An agent token carries no superuser bit; it is bounded by the
                // skills its key was granted, not by the owner's admin rights.
                is_superuser: false,
                tenant: Some(agent.tenant.clone()),
                kind: PrincipalKind::Agent,
                agent_id: Some(agent.agent_id),
            });
        }
        None
    }

    /// Whether this principal may act on a row owned by `owner_id`.
    ///
    /// Superusers pass only when the caller opts in, because "admins see
    /// everything" is a product decision, not a framework one.
    #[must_use]
    pub fn may_access(&self, owner_id: i64, superuser_sees_all: bool) -> bool {
        self.user_id == owner_id || (superuser_sees_all && self.is_superuser)
    }
}

/// The resolved tenant's slug, for middleware that wants [`Principal::tenant`]
/// populated from a cookie session (which carries no slug of its own).
#[derive(Clone, Debug)]
pub struct TenantSlug(pub String);

/// Rejection for the [`Principal`] extractor: 401, never 403 — the request did
/// not say who it is, which is a different failure from saying so and being
/// refused.
#[derive(Debug)]
pub struct Unauthenticated;

impl IntoResponse for Unauthenticated {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, "authentication required").into_response()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = Unauthenticated;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Self::from_parts(parts).ok_or(Unauthenticated)
    }
}

/// [`Principal`] where absence is a valid answer — a page that renders
/// differently when signed in, rather than one that refuses.
#[derive(Clone, Debug)]
pub struct OptionalPrincipal(pub Option<Principal>);

impl<S: Send + Sync> FromRequestParts<S> for OptionalPrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(Principal::from_parts(parts)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> Parts {
        axum::http::Request::builder()
            .body(())
            .expect("request")
            .into_parts()
            .0
    }

    #[test]
    fn unauthenticated_requests_have_no_principal() {
        assert!(Principal::from_parts(&parts()).is_none());
    }

    #[test]
    fn an_authenticated_user_becomes_a_user_principal() {
        let mut p = parts();
        p.extensions.insert(AuthenticatedUser {
            id: 7,
            username: "ada".into(),
            is_superuser: true,
        });
        p.extensions.insert(TenantSlug("acme".into()));
        let principal = Principal::from_parts(&p).expect("principal");
        assert_eq!(principal.user_id, 7);
        assert!(principal.is_superuser);
        assert_eq!(principal.tenant.as_deref(), Some("acme"));
        assert_eq!(principal.kind, PrincipalKind::User);
    }

    #[test]
    fn an_explicit_principal_wins_over_a_derived_one() {
        // An app that resolved its own identity — from a signed header, say —
        // must not have it silently replaced by a lower-precedence source.
        let mut p = parts();
        p.extensions.insert(AuthenticatedUser {
            id: 7,
            username: "ada".into(),
            is_superuser: false,
        });
        p.extensions
            .insert(Principal::user(99, false, Some("acme".into())));
        assert_eq!(Principal::from_parts(&p).expect("principal").user_id, 99);
    }

    #[test]
    fn access_is_ownership_unless_superusers_are_let_through() {
        let owner = Principal::user(7, false, None);
        assert!(owner.may_access(7, false));
        assert!(!owner.may_access(8, false));

        let admin = Principal::user(1, true, None);
        assert!(!admin.may_access(8, false), "opt-in, not automatic");
        assert!(admin.may_access(8, true));
    }
}
