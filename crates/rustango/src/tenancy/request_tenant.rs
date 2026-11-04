//! The tenant a request was routed to, resolved once and read by every layer.
//!
//! `server::Builder`'s dispatcher stores it in the request extensions. A
//! hand-built stack resolves it on first use through the mounted tenant
//! context, and the answer is stored for the layers further in.

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{Extensions, Request};

use super::TenantSlug;

/// Who a request's cache keys, limits and permission checks belong to.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestTenant {
    /// No tenant by design: no tenant context is mounted, the operator
    /// console, or the Builder's unresolved router.
    Global,
    /// The resolved registry org.
    Tenant(TenantSlug),
    /// A tenant context is mounted and no tenant matched: no tenant, but a
    /// page may still vary on an input the resolver ignored.
    Unmatched,
    /// Resolution failed, or the org has no id. Never shares state.
    Unknown,
}

impl RequestTenant {
    /// The answer stored on the request, if the dispatcher or an earlier
    /// layer already resolved it.
    #[must_use]
    pub fn get(extensions: &Extensions) -> Option<&Self> {
        extensions.get::<Self>()
    }

    /// The request's tenant: the stored answer, else a tenant an outer auth
    /// layer bound, else the mounted tenant context's resolver. No context
    /// mounted is [`Self::Global`]. A resolved answer is stored on `parts`.
    pub async fn resolve(parts: &mut Parts) -> Self {
        if let Some(t) = Self::get(&parts.extensions) {
            return t.clone();
        }
        if let Some(t) = parts.extensions.get::<TenantSlug>() {
            return Self::Tenant(t.clone());
        }
        let Some(found) = super::middleware::request_org(parts, &parts.extensions).await else {
            // Not stored: a context mounted further in must still resolve.
            return Self::Global;
        };
        if let Err(e) = &found {
            tracing::warn!(target: "rustango::tenancy", error = %e, "tenant resolution failed");
        }
        let tenant = Self::from_resolution(found);
        parts.extensions.insert(tenant.clone());
        tenant
    }

    /// [`Self::resolve`] for a whole request.
    pub(crate) async fn resolve_request(req: Request<Body>) -> (Request<Body>, Self) {
        let (mut parts, body) = req.into_parts();
        let tenant = Self::resolve(&mut parts).await;
        (Request::from_parts(parts, body), tenant)
    }

    /// The answer for a resolver's result. An org without an id cannot be
    /// told from a later one with its slug, so it is `Unknown`.
    pub(crate) fn from_resolution<E>(found: Result<Option<super::Org>, E>) -> Self {
        match found {
            Ok(Some(org)) => TenantSlug::of(&org).map_or(Self::Unknown, Self::Tenant),
            Ok(None) => Self::Unmatched,
            Err(_) => Self::Unknown,
        }
    }

    /// `true` when [`Self::resolve`] would answer `Global` only because
    /// nothing is mounted or stored. A layer that must never share across
    /// tenants by mistake treats that as `Unknown`.
    #[cfg_attr(not(feature = "cache-page"), allow(dead_code))]
    pub(crate) fn unmounted(extensions: &Extensions) -> bool {
        Self::get(extensions).is_none()
            && extensions.get::<TenantSlug>().is_none()
            && crate::extractors::MountedTenantContext::of(extensions).is_none()
    }

    /// The tenant, when there is one.
    #[must_use]
    pub fn tenant(&self) -> Option<&TenantSlug> {
        match self {
            Self::Tenant(t) => Some(t),
            Self::Global | Self::Unmatched | Self::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> Parts {
        Request::builder().body(()).unwrap().into_parts().0
    }

    #[tokio::test]
    async fn no_context_is_global_and_not_stored() {
        let mut p = parts();
        assert_eq!(RequestTenant::resolve(&mut p).await, RequestTenant::Global);
        assert!(RequestTenant::get(&p.extensions).is_none());
        assert!(RequestTenant::unmounted(&p.extensions));
    }

    #[tokio::test]
    async fn a_stored_answer_wins_over_a_bound_credential() {
        let mut p = parts();
        p.extensions.insert(TenantSlug::new(2, "globex"));
        p.extensions.insert(RequestTenant::Unknown);
        assert_eq!(RequestTenant::resolve(&mut p).await, RequestTenant::Unknown);
        assert!(!RequestTenant::unmounted(&p.extensions));
    }

    #[test]
    fn an_org_without_an_id_is_unknown() {
        let org = crate::tenancy::Org {
            slug: "acme".into(),
            ..crate::testkit::org()
        };
        assert_eq!(
            RequestTenant::from_resolution::<()>(Ok(Some(org))),
            RequestTenant::Unknown
        );
        assert_eq!(
            RequestTenant::from_resolution::<()>(Ok(None)),
            RequestTenant::Unmatched
        );
        assert_eq!(
            RequestTenant::from_resolution(Err(())),
            RequestTenant::Unknown
        );
    }
}
