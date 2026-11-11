//! The tenant view of one root cache for a request: one call, not a
//! `TenantSlug` built by hand at every call site.
//!
//! ```ignore
//! let scope = CacheScope::from_request(&root, &mut parts).await?; // None: tenant unknown
//! let session = scope.sessions().load(&sid).await?;
//! let beta = scope.flags().is_enabled_for("beta", &user_id).await;
//!
//! // A job, for each tenant: every key is `tenant:#org:<id>:<slug>:<area>:…`.
//! let locks = CacheScope::for_tenant(root.clone(), &tenant).locks();
//! ```

use super::keyspace::{Area, KeySpace};
use super::{BoxedCache, CacheError, ScopedCache};
use crate::tenancy::{RequestTenant, TenantSlug};

/// A root cache and the request's tenant. Cheap to clone.
#[derive(Clone)]
pub struct CacheScope {
    root: BoxedCache,
    space: KeySpace,
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
        let space = KeySpace::of_request(parts).await.ok()?;
        Some(Self {
            root: root.clone(),
            space,
        })
    }

    /// One tenant's scope, for a job or the scheduler.
    #[must_use]
    pub fn for_tenant(root: BoxedCache, tenant: &TenantSlug) -> Self {
        Self {
            root,
            space: KeySpace::Tenant(tenant.clone()),
        }
    }

    /// The platform-wide scope: keys as without tenancy.
    #[must_use]
    pub fn global(root: BoxedCache) -> Self {
        Self {
            root,
            space: KeySpace::Global,
        }
    }

    /// The request's tenant; `None` for a global request.
    #[must_use]
    pub fn tenant(&self) -> Option<&TenantSlug> {
        self.space.tenant()
    }

    /// App keys: `tenant:#org:<id>:<slug>:app:<key>`, or bare for global.
    /// A tenant's `clear()` keeps its locks, sessions and markers. A global
    /// scope's `clear()` is refused (`CacheError::Refused`): it would delete
    /// every tenant's keys; call the root cache's `clear()` on purpose.
    #[must_use]
    pub fn app(&self) -> BoxedCache {
        ScopedCache::in_space(self.root.clone(), self.space.clone(), Area::App).boxed()
    }

    /// Locks of this tenant; process-wide for a global scope.
    #[must_use]
    pub fn locks(&self) -> crate::distributed_lock::DistributedLock {
        crate::distributed_lock::DistributedLock::in_space(self.root.clone(), self.space.clone())
    }

    /// Sessions of this tenant; unscoped for a global request.
    #[cfg(feature = "sessions")]
    #[must_use]
    pub fn sessions(&self) -> crate::sessions::SessionStore {
        crate::sessions::SessionStore::new(self.root.clone()).in_space(self.space.clone())
    }

    /// Feature flags with this tenant's per-user overrides.
    #[must_use]
    pub fn flags(&self) -> crate::feature_flags::FeatureFlags {
        crate::feature_flags::FeatureFlags::new(self.root.clone()).in_space(self.space.clone())
    }

    /// Delete every key of this tenant from the root, in every area, plus the
    /// 0.63 lock layout. The 0.63 idempotency copies (`idem:<sha>`, written until
    /// 0.65.0) carry no tenant prefix, so they expire with their TTL instead.
    /// A global scope deletes nothing.
    ///
    /// # Errors
    /// The backend's `delete_prefixes` error.
    pub async fn purge(&self) -> Result<(), CacheError> {
        let prefixes = self.space.purge_prefixes();
        if prefixes.is_empty() {
            return Ok(());
        }
        let refs: Vec<&str> = prefixes.iter().map(String::as_str).collect();
        self.root.delete_prefixes(&refs).await
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
