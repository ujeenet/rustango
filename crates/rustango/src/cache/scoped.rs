//! A cache view that namespaces every key.
//!
//! [`Cache`](super::Cache) is one flat keyspace. Under tenancy that is
//! a trap: a handler, or a background task with no tenant in scope,
//! writes `"stats:monthly"` for one tenant and all the others read it.
//!
//! [`ScopedCache`] wraps any [`BoxedCache`](super::BoxedCache) and
//! folds a namespace into every key, so the call site cannot forget:
//!
//! ```ignore
//! use rustango::cache::CacheScope;
//!
//! // In a handler, from the Org the resolver already produced:
//! let tenant = TenantSlug::of(&t.org).expect("a registry org");
//! let cache = CacheScope::for_tenant(shared_cache.clone(), &tenant).app();
//! cache.set("stats:monthly", &json, ttl).await?;   // stored as `tenant:#org:7:acme:app:stats:monthly`
//!
//! // Invalidate just this tenant:
//! cache.clear().await?;                            // other tenants keep their entries
//! ```
//!
//! `ScopedCache` is itself a `Cache`, so it fits anywhere a
//! `BoxedCache` is taken: `cache_page`, `cache_fragment`, rate
//! limiters, [`crate::distributed_lock::DistributedLock`].
//!
//! ## What it does not do
//!
//! This is a namespace, not a security boundary. Everything still
//! lives in one backend, and code holding the unscoped cache can read
//! any key. The point is that the easy path is the correct one.
//!
//! `clear()` calls [`Cache::delete_prefix`] on the inner cache. A
//! backend that does not implement that method returns an error rather
//! than clearing other namespaces.

use std::sync::Arc;
use std::time::Duration;

use super::keyspace::{Area, KeySpace};
use super::{BoxedCache, Cache, CacheError};

/// The first part of every tenant key: `tenant:#org:<id>:<slug>:<area>:…`.
pub const TENANT_PREFIX: &str = "tenant";

/// A [`Cache`] view that namespaces every key. Cheap to clone.
#[derive(Clone)]
pub struct ScopedCache {
    inner: BoxedCache,
    /// Includes the trailing separator, so mapping a key is one
    /// concat with no formatting choice per call.
    prefix: String,
    /// Set for an area view: [`KeySpace::key`] maps each key.
    space: Option<(KeySpace, Area)>,
}

impl ScopedCache {
    /// Namespace `inner` under any `namespace`.
    ///
    /// An empty namespace is allowed and prefixes keys with just
    /// `":"`. That is still its own keyspace, so an empty slug cannot
    /// collide with unscoped entries by accident.
    #[must_use]
    pub fn new(inner: BoxedCache, namespace: impl AsRef<str>) -> Self {
        Self {
            prefix: format!("{}:", namespace.as_ref()),
            inner,
            space: None,
        }
    }

    /// The keys of `area` in `space`. A global app view keeps keys bare.
    #[cfg(feature = "tenancy")]
    pub(crate) fn in_space(inner: BoxedCache, space: KeySpace, area: Area) -> Self {
        Self {
            prefix: space.key(area, ""),
            inner,
            space: Some((space, area)),
        }
    }

    /// Namespace `inner` for one tenant's app keys: `tenant:#org:<id>:<slug>:app:…`.
    #[cfg(feature = "tenancy")]
    #[deprecated(
        since = "0.64.0",
        note = "use `CacheScope::for_tenant(inner, tenant).app()`; removed in 0.67.0"
    )]
    #[must_use]
    pub fn for_tenant(inner: BoxedCache, tenant: &crate::tenancy::TenantSlug) -> Self {
        Self::in_space(inner, KeySpace::Tenant(tenant.clone()), Area::App)
    }

    /// The prefix every key gets, including its trailing separator.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Wrap in an `Arc` so it can be passed anywhere a
    /// [`BoxedCache`] is expected.
    #[must_use]
    pub fn boxed(self) -> BoxedCache {
        Arc::new(self)
    }

    fn k(&self, key: &str) -> String {
        match &self.space {
            Some((space, area)) => space.key(*area, key),
            None => format!("{}{key}", self.prefix),
        }
    }
}

// Every method maps the key and forwards to the inner cache. The
// trait defaults would be correct, but they would turn a backend's
// native `INCRBY`, `SET NX` and `MGET` into per-key loops that are
// slower and not atomic. Forwarding keeps what the backend offers.
#[async_trait::async_trait]
impl Cache for ScopedCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        self.inner.get(&self.k(key)).await
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        self.inner.set(&self.k(key), value, ttl).await
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<bytes::Bytes>, CacheError> {
        self.inner.get_bytes(&self.k(key)).await
    }

    async fn set_bytes(
        &self,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        self.inner.set_bytes(&self.k(key), value, ttl).await
    }

    async fn set_forever(&self, key: &str, value: &str) -> Result<(), CacheError> {
        self.inner.set_forever(&self.k(key), value).await
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.inner.delete(&self.k(key)).await
    }

    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        self.inner.exists(&self.k(key)).await
    }

    /// Clears **only this namespace**, via [`Cache::delete_prefix`]
    /// on the inner cache. The global app view has no namespace, so it
    /// refuses: that clear would wipe every tenant's keys.
    async fn clear(&self) -> Result<(), CacheError> {
        if self.prefix.is_empty() {
            return Err(CacheError::Refused(
                "the global app cache has no prefix, so clear() would delete every                  tenant's sessions, locks and markers; call clear() on the root cache                  if that is what you mean"
                    .to_owned(),
            ));
        }
        self.inner.delete_prefix(&self.prefix).await
    }

    fn is_process_local(&self) -> bool {
        self.inner.is_process_local()
    }

    fn stores_nothing(&self) -> bool {
        self.inner.stores_nothing()
    }

    async fn incr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        self.inner.incr(&self.k(key), by, ttl).await
    }

    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        self.inner.add(&self.k(key), value, ttl).await
    }

    async fn touch(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        self.inner.touch(&self.k(key), ttl).await
    }

    /// The returned map uses the caller's **unprefixed** keys. The
    /// prefix must not leak back out.
    async fn get_many(
        &self,
        keys: &[&str],
    ) -> Result<std::collections::HashMap<String, String>, CacheError> {
        let scoped: Vec<String> = keys.iter().map(|k| self.k(k)).collect();
        let refs: Vec<&str> = scoped.iter().map(String::as_str).collect();
        let got = self.inner.get_many(&refs).await?;
        // Map back by position: `scoped[i]` corresponds to `keys[i]`.
        let mut out = std::collections::HashMap::with_capacity(got.len());
        for (i, sk) in scoped.iter().enumerate() {
            if let Some(v) = got.get(sk) {
                out.insert(keys[i].to_owned(), v.clone());
            }
        }
        Ok(out)
    }

    async fn set_many(
        &self,
        entries: &[(&str, &str)],
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        let scoped: Vec<(String, &str)> = entries.iter().map(|(k, v)| (self.k(k), *v)).collect();
        let refs: Vec<(&str, &str)> = scoped.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        self.inner.set_many(&refs, ttl).await
    }

    async fn delete_many(&self, keys: &[&str]) -> Result<(), CacheError> {
        let scoped: Vec<String> = keys.iter().map(|k| self.k(k)).collect();
        let refs: Vec<&str> = scoped.iter().map(String::as_str).collect();
        self.inner.delete_many(&refs).await
    }

    /// Scopes nest: `ScopedCache::new(scoped.boxed(), "x")` gives
    /// `"<this>:x:"`.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        self.inner.delete_prefix(&self.k(prefix)).await
    }

    async fn delete_prefixes(&self, prefixes: &[&str]) -> Result<(), CacheError> {
        let scoped: Vec<String> = prefixes.iter().map(|p| self.k(p)).collect();
        let refs: Vec<&str> = scoped.iter().map(String::as_str).collect();
        self.inner.delete_prefixes(&refs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;

    fn mem() -> BoxedCache {
        Arc::new(InMemoryCache::new())
    }

    /// The same logical key in two tenants is two entries.
    #[tokio::test]
    async fn tenants_do_not_see_each_others_keys() {
        let shared = mem();
        let acme = ScopedCache::new(shared.clone(), "acme");
        let globex = ScopedCache::new(shared.clone(), "globex");

        acme.set("stats", "acme-value", None).await.unwrap();

        assert_eq!(
            acme.get("stats").await.unwrap().as_deref(),
            Some("acme-value")
        );
        assert_eq!(
            globex.get("stats").await.unwrap(),
            None,
            "globex must not read acme's entry for the same logical key"
        );
        assert!(!globex.exists("stats").await.unwrap());
    }

    /// `set_forever` beats the inner default TTL, through the scope too.
    #[tokio::test]
    async fn set_forever_outlives_the_default_ttl() {
        let inner: BoxedCache = Arc::new(InMemoryCache::with_default_ttl(
            std::time::Duration::from_millis(1),
        ));
        let scoped = ScopedCache::new(inner.clone(), "acme");
        inner.set_forever("bare", "1").await.unwrap();
        scoped.set_forever("k", "1").await.unwrap();
        inner.set("lapses", "1", None).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(inner.exists("bare").await.unwrap());
        assert!(scoped.exists("k").await.unwrap());
        assert!(!inner.exists("lapses").await.unwrap());
    }

    /// Scoped `clear` drops one tenant and leaves the others.
    #[tokio::test]
    async fn scoped_clear_leaves_other_tenants_intact() {
        let shared = mem();
        let acme = ScopedCache::new(shared.clone(), "acme");
        let globex = ScopedCache::new(shared.clone(), "globex");

        acme.set("a", "1", None).await.unwrap();
        acme.set("b", "2", None).await.unwrap();
        globex.set("a", "9", None).await.unwrap();

        acme.clear().await.unwrap();

        assert_eq!(acme.get("a").await.unwrap(), None);
        assert_eq!(acme.get("b").await.unwrap(), None);
        assert_eq!(
            globex.get("a").await.unwrap().as_deref(),
            Some("9"),
            "clearing acme must not touch globex"
        );
    }

    /// A tenant that reuses a purged tenant's slug gets a fresh keyspace.
    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn a_reused_slug_does_not_see_the_purged_tenants_keys() {
        use crate::tenancy::TenantSlug;
        let shared = mem();
        let app = |id| KeySpace::Tenant(TenantSlug::new(id, "acme"));
        let old = ScopedCache::in_space(shared.clone(), app(1), Area::App);
        let heir = ScopedCache::in_space(shared.clone(), app(2), Area::App);
        old.set("k", "old", None).await.unwrap();
        assert_eq!(heir.get("k").await.unwrap(), None);
    }

    /// The deprecated `for_tenant` writes the new app layout.
    #[cfg(feature = "tenancy")]
    #[allow(deprecated)]
    #[tokio::test]
    async fn for_tenant_wraps_the_app_area() {
        let shared = mem();
        let t = crate::tenancy::TenantSlug::new(1, "acme");
        let app = ScopedCache::for_tenant(shared.clone(), &t);
        assert_eq!(app.prefix(), "tenant:#org:1:acme:app:");
        app.set("k", "v", None).await.unwrap();
        assert!(shared.exists("tenant:#org:1:acme:app:k").await.unwrap());
    }

    /// A global app view keeps keys bare but escapes `tenant:`, and refuses a
    /// clear that would wipe every tenant.
    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn the_global_app_view_cannot_write_a_tenant_key() {
        let shared = mem();
        let app = ScopedCache::in_space(shared.clone(), KeySpace::Global, Area::App);
        app.set("stats", "1", None).await.unwrap();
        app.set("tenant:#org:1:acme:app:k", "forged", None)
            .await
            .unwrap();
        assert!(shared.exists("stats").await.unwrap());
        assert!(!shared.exists("tenant:#org:1:acme:app:k").await.unwrap());
        assert_eq!(
            app.get("tenant:#org:1:acme:app:k")
                .await
                .unwrap()
                .as_deref(),
            Some("forged")
        );
        assert!(matches!(app.clear().await, Err(CacheError::Refused(_))));
        assert!(
            shared.exists("stats").await.unwrap(),
            "a global clear wiped the root"
        );
    }

    /// Clearing `acme:` must not hit `acme-corp:`.
    #[tokio::test]
    async fn prefix_overlap_between_slugs_is_not_a_collision() {
        let shared = mem();
        let acme = ScopedCache::new(shared.clone(), "acme");
        let acme_corp = ScopedCache::new(shared.clone(), "acme-corp");

        acme.set("k", "short", None).await.unwrap();
        acme_corp.set("k", "long", None).await.unwrap();

        acme.clear().await.unwrap();

        assert_eq!(acme.get("k").await.unwrap(), None);
        assert_eq!(
            acme_corp.get("k").await.unwrap().as_deref(),
            Some("long"),
            "the trailing separator must keep `acme` from matching `acme-corp`"
        );
    }

    /// `get_many` maps results back to the caller's unprefixed keys.
    #[tokio::test]
    async fn get_many_returns_unprefixed_keys() {
        let acme = ScopedCache::new(mem(), "acme");
        acme.set_many(&[("x", "1"), ("y", "2")], None)
            .await
            .unwrap();

        let got = acme.get_many(&["x", "y", "absent"]).await.unwrap();
        assert_eq!(got.get("x").map(String::as_str), Some("1"));
        assert_eq!(got.get("y").map(String::as_str), Some("2"));
        assert!(!got.contains_key("absent"));
        assert!(
            got.keys().all(|k| !k.contains("acme:")),
            "the prefix must not leak back to the caller: {got:?}"
        );
    }

    /// Counters are namespaced, so two tenants rate-limiting on the
    /// same key do not share a budget.
    #[tokio::test]
    async fn counters_are_namespaced() {
        let shared = mem();
        let acme = ScopedCache::new(shared.clone(), "acme");
        let globex = ScopedCache::new(shared.clone(), "globex");

        assert_eq!(acme.incr("hits", 1, None).await.unwrap(), 1);
        assert_eq!(acme.incr("hits", 1, None).await.unwrap(), 2);
        assert_eq!(
            globex.incr("hits", 1, None).await.unwrap(),
            1,
            "globex starts its own count"
        );
    }

    /// `delete_many` and `delete` go through the prefix as well.
    #[tokio::test]
    async fn deletes_are_namespaced() {
        let shared = mem();
        let acme = ScopedCache::new(shared.clone(), "acme");
        let globex = ScopedCache::new(shared.clone(), "globex");

        acme.set_many(&[("p", "1"), ("q", "2")], None)
            .await
            .unwrap();
        globex
            .set_many(&[("p", "8"), ("q", "9")], None)
            .await
            .unwrap();

        acme.delete_many(&["p", "q"]).await.unwrap();

        assert_eq!(acme.get("p").await.unwrap(), None);
        assert_eq!(globex.get("p").await.unwrap().as_deref(), Some("8"));
        assert_eq!(globex.get("q").await.unwrap().as_deref(), Some("9"));
    }

    /// `NullCache` stores nothing, so a scoped clear must succeed
    /// rather than hit the erroring trait default.
    #[tokio::test]
    async fn null_cache_prefix_delete_is_a_noop() {
        let null: BoxedCache = Arc::new(crate::cache::NullCache);
        let scoped = ScopedCache::new(null, "acme");
        scoped.clear().await.unwrap();
        assert_eq!(scoped.get("anything").await.unwrap(), None);
    }
}
