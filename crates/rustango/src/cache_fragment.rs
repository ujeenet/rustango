//! Fragment caching — Django's `{% cache %}` template tag.
//!
//! Tera doesn't support custom block-tag extensions (only filters
//! and functions). The classic Django shape
//! `{% cache 500 sidebar %}…{% endcache %}` therefore can't be a
//! 1:1 port — the rendered fragment needs to be cached at handler
//! time, not template time.
//!
//! This module ships [`cached_render`], the handler-side equivalent:
//! a small wrapper that consults the [`crate::cache::Cache`] before
//! invoking a compute closure, stores the result, and returns the
//! cached or freshly-computed fragment on the next call.
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
//! ## Why a handler-side helper, not a Tera tag?
//!
//! - Tera's extension API exposes filters + functions. Filters
//!   transform a value; functions return a value. Neither can wrap
//!   a chunk of template source that should evaluate lazily.
//! - Django's `{% cache %}` works because Django's template engine
//!   compiles to a tree of nodes; the block tag stashes an unevaluated
//!   subtree it renders only on miss. Tera's parser doesn't expose
//!   subtree handles for user code.
//! - The handler-side shape composes naturally with axum's response
//!   pipeline: render the cacheable fragment in your handler, pass
//!   it to the template as a context variable, and the template
//!   just renders the already-cached HTML.
//!
//! ## Behaviour on cache errors
//!
//! Cache backend failures (network timeout, malformed entry) are
//! swallowed and the compute closure runs as if it were a miss.
//! Fragment caching is an optimization — surfacing a 500 because
//! Redis is briefly unreachable would be the wrong default. The
//! cache failure is logged via `tracing::warn` so operators see it.
//!
//! Issue #16.

use std::time::Duration;

use crate::cache::Cache;

/// Return the cached fragment for `key`, or compute + store it.
///
/// `compute` is invoked **only on miss** (or on cache error). The
/// returned `String` is the value to render — pass it to your
/// template as a context variable (`{{ sidebar_html | safe }}`).
///
/// `ttl` follows the `Cache::set` convention: `None` means "no
/// expiry" (store indefinitely), `Some(dur)` is the per-key TTL.
///
/// Cache backend failures degrade silently — the compute closure
/// runs and its output is returned to the caller. The failure is
/// logged via `tracing::warn` so it's visible in production
/// observability.
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
        // Stored — confirm via direct cache get.
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
        // InMemoryCache honours TTL on get.
        assert!(
            cache.get("k3").await.unwrap().is_none(),
            "entry should have expired"
        );
    }

    // A test cache backend whose every method errors — pin the
    // graceful-degradation behaviour.
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
        // Same backend — both get and set fail. The caller still
        // gets a usable string.
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
