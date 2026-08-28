//! Key-namespaced cache view (#1227).
//!
//! [`Cache`](super::Cache) is a flat `&str`-keyed store. That is the
//! right primitive, but under tenancy it is a footgun: the natural key
//! is the leaky key. A handler (or worse, a background task with no
//! ambient tenant) writes `"stats:monthly"` for one tenant and every
//! other tenant reads it back.
//!
//! [`ScopedCache`] closes that by construction — it wraps any
//! [`BoxedCache`](super::BoxedCache) and folds a namespace into every
//! key on the way through, so the call site cannot forget:
//!
//! ```ignore
//! use rustango::cache::ScopedCache;
//!
//! // In a handler, from the Org the resolver already produced:
//! let cache = ScopedCache::for_tenant(shared_cache.clone(), &t.org.slug);
//! cache.set("stats:monthly", &json, ttl).await?;   // stored as `tenant:acme:stats:monthly`
//!
//! // Invalidate just this tenant:
//! cache.clear().await?;                            // other tenants keep their entries
//! ```
//!
//! `ScopedCache` is itself a `Cache`, so it drops into anything that
//! takes a `BoxedCache` — `cache_page`, `cache_fragment`, rate limiters,
//! [`crate::distributed_lock::DistributedLock`].
//!
//! ## What it does not do
//!
//! It is a namespace, not a security boundary: everything still lives in
//! one backend, and code holding the *unscoped* cache can read any key.
//! The point is that the ergonomic path is the correct one.
//!
//! `clear()` routes through [`Cache::delete_prefix`], whose default
//! over-deletes on backends that cannot enumerate keys (see that
//! method's docs). Namespaced state stays correct either way; on those
//! backends the other namespaces just pay a cache miss.

use std::sync::Arc;
use std::time::Duration;

use super::{BoxedCache, Cache, CacheError};

/// The prefix [`ScopedCache::for_tenant`] uses, matching the
/// `tenant:{slug}:…` convention the tenancy layer already uses for
/// lockout keys (`tenancy::auth_routes`).
pub const TENANT_PREFIX: &str = "tenant";

/// A [`Cache`] view that transparently namespaces every key.
///
/// Cheap to clone (the inner cache is an `Arc`).
#[derive(Clone)]
pub struct ScopedCache {
    inner: BoxedCache,
    /// Already includes the trailing separator, so key mapping is one
    /// concat with no per-call formatting decisions.
    prefix: String,
}

impl ScopedCache {
    /// Namespace `inner` under an arbitrary `namespace`.
    ///
    /// An empty namespace is accepted and yields keys prefixed with just
    /// `":"` — deliberately still distinct from the unscoped keyspace, so
    /// an accidentally-empty slug cannot silently collide with unscoped
    /// entries.
    #[must_use]
    pub fn new(inner: BoxedCache, namespace: impl AsRef<str>) -> Self {
        Self {
            prefix: format!("{}:", namespace.as_ref()),
            inner,
        }
    }

    /// Namespace `inner` for one tenant slug — `tenant:{slug}:…`.
    #[must_use]
    pub fn for_tenant(inner: BoxedCache, slug: impl AsRef<str>) -> Self {
        Self::new(inner, format!("{TENANT_PREFIX}:{}", slug.as_ref()))
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
        format!("{}{key}", self.prefix)
    }
}

// Every method forwards to the inner cache with a mapped key rather than
// relying on the trait defaults. The defaults would also be *correct*
// (they route back through `self`), but they would flatten a backend's
// native primitives — Redis `INCRBY` / `SET NX` / `MGET` — into
// non-atomic, one-RTT-per-key loops. Forwarding keeps whatever the inner
// backend actually implements.
#[async_trait::async_trait]
impl Cache for ScopedCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        self.inner.get(&self.k(key)).await
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        self.inner.set(&self.k(key), value, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.inner.delete(&self.k(key)).await
    }

    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        self.inner.exists(&self.k(key)).await
    }

    /// Clears **only this namespace** — the whole point of the type.
    /// Delegates to [`Cache::delete_prefix`] on the inner cache.
    async fn clear(&self) -> Result<(), CacheError> {
        self.inner.delete_prefix(&self.prefix).await
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

    /// The returned map is keyed by the caller's **unprefixed** keys —
    /// the prefix is an implementation detail that must not leak back
    /// out.
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

    /// Nested scoping composes: the outer prefix is applied on top of
    /// this one, so `ScopedCache::new(scoped.boxed(), "x")` behaves as
    /// `"<this>:x:"`.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        self.inner.delete_prefix(&self.k(prefix)).await
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
        let acme = ScopedCache::for_tenant(shared.clone(), "acme");
        let globex = ScopedCache::for_tenant(shared.clone(), "globex");

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

    /// Scoped `clear` drops one tenant and leaves the others — the
    /// behaviour the flat `Cache::clear` could not give.
    #[tokio::test]
    async fn scoped_clear_leaves_other_tenants_intact() {
        let shared = mem();
        let acme = ScopedCache::for_tenant(shared.clone(), "acme");
        let globex = ScopedCache::for_tenant(shared.clone(), "globex");

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

    /// A slug that is a prefix of another must not be caught by the
    /// other's clear — `tenant:acme:` vs `tenant:acme-corp:`.
    #[tokio::test]
    async fn prefix_overlap_between_slugs_is_not_a_collision() {
        let shared = mem();
        let acme = ScopedCache::for_tenant(shared.clone(), "acme");
        let acme_corp = ScopedCache::for_tenant(shared.clone(), "acme-corp");

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
        let acme = ScopedCache::for_tenant(mem(), "acme");
        acme.set_many(&[("x", "1"), ("y", "2")], None)
            .await
            .unwrap();

        let got = acme.get_many(&["x", "y", "absent"]).await.unwrap();
        assert_eq!(got.get("x").map(String::as_str), Some("1"));
        assert_eq!(got.get("y").map(String::as_str), Some("2"));
        assert!(!got.contains_key("absent"));
        assert!(
            got.keys().all(|k| !k.contains("tenant:")),
            "the prefix must not leak back to the caller: {got:?}"
        );
    }

    /// Counters are namespaced too — two tenants rate-limiting on the
    /// same logical key must not share a budget.
    #[tokio::test]
    async fn counters_are_namespaced() {
        let shared = mem();
        let acme = ScopedCache::for_tenant(shared.clone(), "acme");
        let globex = ScopedCache::for_tenant(shared.clone(), "globex");

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
        let acme = ScopedCache::for_tenant(shared.clone(), "acme");
        let globex = ScopedCache::for_tenant(shared.clone(), "globex");

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

    /// `NullCache` must not hit the trait default's whole-cache clear
    /// path (it has nothing to enumerate, and the warning would be
    /// noise on every invalidation).
    #[tokio::test]
    async fn null_cache_prefix_delete_is_a_noop() {
        let null: BoxedCache = Arc::new(crate::cache::NullCache);
        let scoped = ScopedCache::for_tenant(null, "acme");
        scoped.clear().await.unwrap();
        assert_eq!(scoped.get("anything").await.unwrap(), None);
    }
}
