//! Fragment caching: cache one rendered piece of a page.
//!
//! Tera has no custom block tags, only filters and functions, so a
//! lazy `{% cache 500 sidebar %}…{% endcache %}` block is not
//! possible. Instead, cache the fragment in your handler with
//! [`cached_render`] and pass the string into the template.
//!
//! ```ignore
//! use std::time::Duration;
//! use rustango::cache::Cache;
//! use rustango::cache_fragment::cached_render;
//!
//! async fn render_sidebar(cache: &dyn Cache) -> String {
//!     cached_render(
//!         cache,
//!         "sidebar:articles",
//!         Some(Duration::from_secs(300)),
//!         || async {
//!             // heavy work — happens only on miss
//!             render_expensive_sidebar().await
//!         },
//!     )
//!     .await
//! }
//! ```
//!
//! ## Cache errors
//!
//! A backend failure is treated as a miss: the closure runs and the
//! caller still gets a value. Fragment caching is only a speed-up, so
//! a brief Redis outage should not turn into a 500. Failures are
//! logged with `tracing::warn`.
//!
//! ## Security
//!
//! The key is yours to choose. If a fragment shows per-user or
//! per-tenant data, put the user or tenant id in the key, or one
//! viewer's HTML will be served to another.
//!
//! [`cached_render`]: crate::cache_fragment::cached_render

use std::time::Duration;

use crate::cache::Cache;

/// Return the cached fragment for `key`, or compute and store it.
///
/// `compute` runs only on a miss or a cache error. Pass the result to
/// your template as a variable, such as `{{ sidebar_html | safe }}`.
///
/// `ttl` follows `Cache::set`: `None` stores with no expiry.
///
/// A cache error is logged and treated as a miss, so the caller always
/// gets a value.
pub async fn cached_render<F, Fut>(
    cache: &dyn Cache,
    key: &str,
    ttl: Option<Duration>,
    compute: F,
) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = String>,
{
    match cache.get(key).await {
        Ok(Some(hit)) => return hit,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                cache_key = %key,
                error = %e,
                "cache_fragment: backend `get` failed; recomputing",
            );
        }
    }
    let computed = compute().await;
    if let Err(e) = cache.set(key, &computed, ttl).await {
        tracing::warn!(
            cache_key = %key,
            error = %e,
            "cache_fragment: backend `set` failed; serving fresh value anyway",
        );
    }
    computed
}

/// Build a stable cache key from a fragment name and the values the
/// fragment varies on.
///
/// Shape: `template.cache.{name}.{hash}`. Order matters, so
/// `["a", "b"]` and `["b", "a"]` give different keys.
///
/// Use it with [`cached_render`] to invalidate a fragment by hand:
///
/// ```ignore
/// use rustango::cache_fragment::make_template_fragment_key;
///
/// // Build the key for the per-user sidebar fragment.
/// let key = make_template_fragment_key("sidebar", &[&user_id.to_string()]);
/// cache.delete(&key).await?;  // invalidate when underlying data changes
/// ```
#[must_use]
pub fn make_template_fragment_key(fragment_name: &str, vary_on: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut joined = String::with_capacity(64);
    for (i, part) in vary_on.iter().enumerate() {
        if i > 0 {
            joined.push(':');
        }
        joined.push_str(part);
    }
    // SHA-256 cut to 32 hex chars: a short key, and no extra
    // dependency for a weaker hash.
    let digest = Sha256::digest(joined.as_bytes());
    let hex: String = digest.iter().take(16).fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    format!("template.cache.{fragment_name}.{hex}")
}

#[cfg(test)]
mod fragment_key_tests {
    use super::*;

    #[test]
    fn key_includes_template_cache_prefix_and_fragment_name() {
        let k = make_template_fragment_key("sidebar", &[]);
        assert!(k.starts_with("template.cache.sidebar."));
    }

    #[test]
    fn key_is_deterministic_for_same_inputs() {
        let a = make_template_fragment_key("sidebar", &["42", "en"]);
        let b = make_template_fragment_key("sidebar", &["42", "en"]);
        assert_eq!(a, b);
    }

    #[test]
    fn key_changes_with_vary_on_values() {
        let a = make_template_fragment_key("sidebar", &["42"]);
        let b = make_template_fragment_key("sidebar", &["43"]);
        assert_ne!(a, b);
    }

    #[test]
    fn key_changes_with_fragment_name() {
        let a = make_template_fragment_key("sidebar", &["42"]);
        let b = make_template_fragment_key("header", &["42"]);
        assert_ne!(a, b);
    }

    #[test]
    fn key_empty_vary_on_works() {
        // No vary_on still gives a stable key.
        let k = make_template_fragment_key("static_block", &[]);
        assert!(k.starts_with("template.cache.static_block."));
    }

    #[test]
    fn key_hash_segment_is_32_hex_chars() {
        let k = make_template_fragment_key("x", &["1"]);
        let hash = k.rsplit('.').next().unwrap();
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn key_order_of_vary_on_matters() {
        // vary_on is ordered, not a set.
        let a = make_template_fragment_key("x", &["a", "b"]);
        let b = make_template_fragment_key("x", &["b", "a"]);
        assert_ne!(a, b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Cache, InMemoryCache};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn miss_invokes_compute_and_stores() {
        let cache = InMemoryCache::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let out = cached_render(&cache, "k1", None, move || {
            let calls = Arc::clone(&calls_clone);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                "computed".to_owned()
            }
        })
        .await;
        assert_eq!(out, "computed");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // It really landed in the cache.
        let stored = cache.get("k1").await.unwrap();
        assert_eq!(stored.as_deref(), Some("computed"));
    }

    #[tokio::test]
    async fn hit_returns_cached_without_recomputing() {
        let cache = InMemoryCache::new();
        cache.set("k2", "from-cache", None).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let out = cached_render(&cache, "k2", None, move || {
            let calls = Arc::clone(&calls_clone);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                "recomputed".to_owned()
            }
        })
        .await;
        assert_eq!(out, "from-cache");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "compute should not run on hit"
        );
    }

    #[tokio::test]
    async fn ttl_is_passed_through_to_backend() {
        let cache = InMemoryCache::new();
        cached_render(&cache, "k3", Some(Duration::from_millis(20)), || async {
            "with-ttl".to_owned()
        })
        .await;
        assert_eq!(cache.get("k3").await.unwrap().as_deref(), Some("with-ttl"));
        tokio::time::sleep(Duration::from_millis(30)).await;
        // InMemoryCache checks the TTL on get.
        assert!(
            cache.get("k3").await.unwrap().is_none(),
            "entry should have expired"
        );
    }

    // A backend where get and set always fail.
    struct ExplodingCache;
    #[async_trait]
    impl Cache for ExplodingCache {
        async fn get(&self, _: &str) -> Result<Option<String>, crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("boom".into()))
        }
        async fn set(
            &self,
            _: &str,
            _: &str,
            _: Option<Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("boom".into()))
        }
        async fn delete(&self, _: &str) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }
        async fn exists(&self, _: &str) -> Result<bool, crate::cache::CacheError> {
            Ok(false)
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn cache_get_failure_recomputes_and_returns() {
        let cache = ExplodingCache;
        let out = cached_render(&cache, "k4", None, || async { "fresh".to_owned() }).await;
        assert_eq!(out, "fresh");
    }

    #[tokio::test]
    async fn cache_set_failure_still_returns_computed_value() {
        // Both get and set fail; the caller still gets a string.
        let cache = ExplodingCache;
        let out = cached_render(&cache, "k5", Some(Duration::from_secs(60)), || async {
            "still-fresh".to_owned()
        })
        .await;
        assert_eq!(out, "still-fresh");
    }

    #[tokio::test]
    async fn compute_runs_only_once_for_repeated_hits() {
        let cache = InMemoryCache::new();
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let calls_clone = Arc::clone(&calls);
            let _ = cached_render(&cache, "k6", None, move || {
                let calls = Arc::clone(&calls_clone);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "x".to_owned()
                }
            })
            .await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "compute ran once across 5 calls"
        );
    }
}
