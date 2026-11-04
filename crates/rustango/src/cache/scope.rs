//! The tenant view of one root cache for a request: one call, not a
//! `TenantSlug` built by hand at every call site.
//!
//! ```ignore
//! let scope = CacheScope::from_request(&root, &mut parts).await?; // None: tenant unknown
//! let session = scope.sessions().load(&sid).await?;
//! let beta = scope.flags().is_enabled_for("beta", &user_id).await;
//! ```

use super::{BoxedCache, ScopedCache};
use crate::tenancy::{RequestTenant, TenantSlug};

/// A root cache and the request's tenant. Cheap to clone.
#[derive(Clone)]
pub struct CacheScope {
    root: BoxedCache,
    tenant: Option<TenantSlug>,
}

impl CacheScope {
    /// The scope of the request's [`RequestTenant`]: global when no tenant
    /// matched. `None` when resolution failed, or when no tenant context is
    /// mounted or stored: nothing may be shared then. A single-db app uses
    /// `SessionStore::new(root)` and `FeatureFlags::new(root)` directly.
    pub async fn from_request(
        root: &BoxedCache,
        parts: &mut axum::http::request::Parts,
    ) -> Option<Self> {
        if RequestTenant::unmounted(&parts.extensions) {
            return None;
        }
        let tenant = match RequestTenant::resolve(parts).await {
            RequestTenant::Tenant(t) => Some(t),
            RequestTenant::Unknown => return None,
            _ => None,
        };
        Some(Self {
            root: root.clone(),
            tenant,
        })
    }

    /// The request's tenant; `None` for a global request.
    #[must_use]
    pub fn tenant(&self) -> Option<&TenantSlug> {
        self.tenant.as_ref()
    }

    /// App keys: [`ScopedCache::for_tenant`] for a tenant, the root otherwise.
    #[must_use]
    pub fn app(&self) -> BoxedCache {
        match &self.tenant {
            Some(t) => ScopedCache::for_tenant(self.root.clone(), t).boxed(),
            None => self.root.clone(),
        }
    }

    /// Sessions of this tenant; unscoped for a global request.
    #[cfg(feature = "sessions")]
    #[must_use]
    pub fn sessions(&self) -> crate::sessions::SessionStore {
        let store = crate::sessions::SessionStore::new(self.root.clone());
        match &self.tenant {
            Some(t) => store.for_tenant(t),
            None => store,
        }
    }

    /// Feature flags with this tenant's per-user overrides.
    #[must_use]
    pub fn flags(&self) -> crate::feature_flags::FeatureFlags {
        let flags = crate::feature_flags::FeatureFlags::new(self.root.clone());
        match &self.tenant {
            Some(t) => flags.for_tenant(t),
            None => flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No tenant context mounted: no scope, so a session cannot load across hosts.
    #[tokio::test]
    async fn no_context_gives_no_scope() {
        let root: BoxedCache = std::sync::Arc::new(crate::cache::InMemoryCache::new());
        let mut parts = axum::http::Request::new(()).into_parts().0;
        assert!(CacheScope::from_request(&root, &mut parts).await.is_none());
        parts.extensions.insert(RequestTenant::Global);
        assert!(CacheScope::from_request(&root, &mut parts).await.is_some());
    }
}
