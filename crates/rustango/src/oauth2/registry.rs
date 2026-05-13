//! Per-tenant OAuth2 provider registry.
//!
//! Map `(tenant_id, provider_name)` to a configured [`OAuth2Provider`].
//! Two backends ship out of the box:
//!
//! - **In-memory** ([`OAuth2Registry`]) — config-driven, populated at
//!   startup from `config/{env}.toml` or env vars. Hot-reload not
//!   supported (rebuild the registry, swap the `Arc`).
//!
//! - **DB-backed** — wrap your own table behind the [`ProviderLoader`]
//!   trait so tenants can rotate keys from the admin without redeploys.
//!   See [`ProviderLoader`] for the contract; rustango doesn't ship a
//!   default schema (that's app-level).
//!
//! ## Single-tenant
//!
//! Use the empty string for `tenant`:
//! ```ignore
//! let mut reg = OAuth2Registry::new();
//! reg.register("", providers::google(...));
//! reg.register("", providers::github(...));
//! ```

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::OAuth2Provider;

/// In-memory registry, keyed by `(tenant_id, provider_name)`. `Arc`-wrapped
/// internally so cheap to clone and pass into axum state.
#[derive(Clone, Default)]
pub struct OAuth2Registry {
    inner: Arc<RwLock<HashMap<(String, String), Arc<OAuth2Provider>>>>,
}

impl OAuth2Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` under `(tenant, provider.name)`.
    pub fn register(&self, tenant: impl Into<String>, provider: OAuth2Provider) {
        let name = provider.name.clone();
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert((tenant.into(), name), Arc::new(provider));
    }

    /// Drop a provider. Returns `true` if something was removed.
    pub fn deregister(&self, tenant: &str, name: &str) -> bool {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(tenant.to_owned(), name.to_owned()))
            .is_some()
    }

    /// Look up the provider for `(tenant, name)`. Returns `None` when
    /// either the tenant or the provider isn't configured.
    #[must_use]
    pub fn get(&self, tenant: &str, name: &str) -> Option<Arc<OAuth2Provider>> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(tenant.to_owned(), name.to_owned()))
            .cloned()
    }

    /// Return all `(tenant, provider_name)` pairs currently registered.
    #[must_use]
    pub fn list(&self) -> Vec<(String, String)> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Return the configured provider names for a tenant.
    #[must_use]
    pub fn list_for_tenant(&self, tenant: &str) -> Vec<String> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|((t, _), _)| t == tenant)
            .map(|((_, n), _)| n.clone())
            .collect()
    }

    /// Drop everything. Useful for tests or hot-reload.
    pub fn clear(&self) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// Async loader for DB-backed registries.
///
/// Implement this on your repo type (e.g. `ProviderRepo` that wraps a
/// `sqlx::PgPool`) to source provider configs from a per-tenant table —
/// great for SaaS where each customer brings their own Google/Microsoft
/// app. Combine with a small TTL cache if your loader hits the DB.
#[async_trait::async_trait]
pub trait ProviderLoader: Send + Sync + 'static {
    async fn load(
        &self,
        tenant: &str,
        provider_name: &str,
    ) -> Result<Option<OAuth2Provider>, super::OAuthError>;
}

/// Cache wrapper — checks the in-memory registry first, falls back to
/// `loader` and caches the result. Use this for DB-backed setups so the
/// hot path stays an in-memory map lookup.
#[derive(Clone)]
pub struct CachedRegistry<L: ProviderLoader> {
    pub registry: OAuth2Registry,
    pub loader: Arc<L>,
}

impl<L: ProviderLoader> CachedRegistry<L> {
    pub fn new(loader: L) -> Self {
        Self {
            registry: OAuth2Registry::new(),
            loader: Arc::new(loader),
        }
    }

    /// Look up `(tenant, name)` in the cache; on miss, ask the loader
    /// and cache the result.
    pub async fn get(
        &self,
        tenant: &str,
        name: &str,
    ) -> Result<Option<Arc<OAuth2Provider>>, super::OAuthError> {
        if let Some(p) = self.registry.get(tenant, name) {
            return Ok(Some(p));
        }
        match self.loader.load(tenant, name).await? {
            Some(p) => {
                self.registry.register(tenant.to_owned(), p);
                Ok(self.registry.get(tenant, name))
            }
            None => Ok(None),
        }
    }

    /// Drop the cached entry — call after the user updates the provider
    /// config in the admin so the next request re-loads it.
    pub fn invalidate(&self, tenant: &str, name: &str) {
        self.registry.deregister(tenant, name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth2::providers;

    #[test]
    fn register_and_lookup_in_memory() {
        let reg = OAuth2Registry::new();
        reg.register("acme", providers::google("cid", "csec", "https://acme/cb"));
        let p = reg.get("acme", "google").unwrap();
        assert_eq!(p.name, "google");
        assert_eq!(p.client_id, "cid");
    }

    #[test]
    fn unknown_tenant_or_provider_returns_none() {
        let reg = OAuth2Registry::new();
        reg.register("acme", providers::google("cid", "csec", "https://acme/cb"));
        assert!(reg.get("other", "google").is_none());
        assert!(reg.get("acme", "github").is_none());
    }

    #[test]
    fn list_for_tenant_returns_only_that_tenant() {
        let reg = OAuth2Registry::new();
        reg.register("a", providers::google("cid", "csec", "https://a/cb"));
        reg.register("a", providers::github("cid", "csec", "https://a/cb"));
        reg.register("b", providers::google("cid", "csec", "https://b/cb"));
        let mut names = reg.list_for_tenant("a");
        names.sort();
        assert_eq!(names, vec!["github".to_owned(), "google".to_owned()]);
        assert_eq!(reg.list_for_tenant("b"), vec!["google".to_owned()]);
    }

    #[test]
    fn deregister_removes_entry() {
        let reg = OAuth2Registry::new();
        reg.register("acme", providers::google("cid", "csec", "https://acme/cb"));
        assert!(reg.deregister("acme", "google"));
        assert!(reg.get("acme", "google").is_none());
        assert!(!reg.deregister("acme", "google"));
    }

    #[test]
    fn clear_empties_registry() {
        let reg = OAuth2Registry::new();
        reg.register("acme", providers::google("cid", "csec", "https://acme/cb"));
        reg.register("acme", providers::github("cid", "csec", "https://acme/cb"));
        reg.clear();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn registry_clone_shares_state() {
        let reg = OAuth2Registry::new();
        let clone = reg.clone();
        reg.register("acme", providers::google("cid", "csec", "https://acme/cb"));
        assert!(clone.get("acme", "google").is_some());
    }

    // ----------------------------------------------------------- DB-backed loader

    struct StaticLoader {
        provider: OAuth2Provider,
    }

    #[async_trait::async_trait]
    impl ProviderLoader for StaticLoader {
        async fn load(
            &self,
            tenant: &str,
            name: &str,
        ) -> Result<Option<OAuth2Provider>, super::super::OAuthError> {
            if tenant == "acme" && name == "google" {
                // Re-build a fresh provider per call (loader contract)
                Ok(Some(providers::google(
                    self.provider.client_id.clone(),
                    self.provider.client_secret.clone(),
                    self.provider.redirect_uri.clone(),
                )))
            } else {
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn cached_registry_loads_on_miss() {
        let cached = CachedRegistry::new(StaticLoader {
            provider: providers::google("cid", "csec", "https://acme/cb"),
        });
        // First lookup — miss, hits loader.
        let p = cached.get("acme", "google").await.unwrap().unwrap();
        assert_eq!(p.client_id, "cid");
        // Second lookup — should hit cache (we can't observe directly,
        // but `list()` shows it's there).
        assert_eq!(cached.registry.list().len(), 1);
    }

    #[tokio::test]
    async fn cached_registry_returns_none_for_unknown() {
        let cached = CachedRegistry::new(StaticLoader {
            provider: providers::google("cid", "csec", "https://acme/cb"),
        });
        assert!(cached.get("other", "google").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn invalidate_drops_cache_entry() {
        let cached = CachedRegistry::new(StaticLoader {
            provider: providers::google("cid", "csec", "https://acme/cb"),
        });
        cached.get("acme", "google").await.unwrap();
        cached.invalidate("acme", "google");
        assert!(cached.registry.get("acme", "google").is_none());
    }
}
