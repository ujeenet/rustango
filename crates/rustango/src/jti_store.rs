//! Pluggable JTI (JWT ID) revocation / single-use store.
//!
//! A `JtiStore` records which JWT ids are spent: revoked at logout, or
//! consumed by a single-use impersonation handoff. A request carrying
//! a spent `jti` must be rejected, or the token can be replayed until
//! it expires.
//!
//! Anything the store forgets becomes valid again. A restart or an
//! eviction can drop an entry. A store that is not shared never sees
//! the other processes. In both cases a revoked `jti` still works
//! until its `exp`. Use a shared, durable store once you run more
//! than one instance.
//!
//! - [`InMemoryJtiStore`] — process-local map, the default for every
//!   shipped consumer. Good for one instance, dev and tests.
//!   `mark_used` prunes expired entries on each call, so memory stays
//!   bounded with no background sweeper.
//! - Anything else — implement [`JtiStore`] over Redis, a database
//!   table, or another shared store.
//!   [`RedisCache`](crate::cache::redis_backend::RedisCache) is the
//!   smallest bridge.
//!
//! [`InMemoryJtiStore`]: crate::jti_store::InMemoryJtiStore
//! [`JtiStore`]: crate::jti_store::JtiStore

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

/// Future returned by the [`JtiStore`] methods.
///
/// Boxed because consumers hold the store as `Arc<dyn JtiStore>`, and
/// `async fn` in a trait is not dyn-compatible. Written by hand rather
/// than with `#[async_trait]` so this ungated module still builds when
/// the optional `async-trait` dependency is off.
pub type JtiFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Storage backend for "this JTI is no longer valid" lookups.
///
/// Implementations MUST make `mark_used` atomic: for one `jti`, exactly
/// one concurrent caller may see `true` and every other must see
/// `false`. Otherwise two requests can spend the same single-use token.
///
/// The methods return futures so a durable store can `await` its write.
/// Revoking is the one operation you want to be immediate everywhere; a
/// store that buffers writes keeps accepting a revoked `jti` on other
/// instances until it catches up.
///
/// A synchronous implementation just wraps the body:
///
/// ```ignore
/// impl JtiStore for MyStore {
///     fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool> {
///         Box::pin(async move { self.map.lock().unwrap().contains_key(jti) })
///     }
///     fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
///         Box::pin(async move { /* … */ true })
///     }
/// }
/// ```
///
/// …and a durable one is a single query:
///
/// ```ignore
/// fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
///     Box::pin(async move {
///         // INSERT … ON CONFLICT DO NOTHING — one round trip, atomic,
///         // and visible to every instance immediately.
///         rows_affected == 1
///     })
/// }
/// ```
pub trait JtiStore: Send + Sync {
    /// Returns `true` if `jti` has previously been marked used /
    /// blacklisted. Read-only; never mutates the store.
    fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool>;

    /// Atomically check and record. Returns `true` when the caller is
    /// the first to use this `jti`, `false` when it was already there
    /// (a replay — reject the request).
    ///
    /// `exp_unix` is the JWT's `exp` claim (unix seconds). Stores MAY
    /// use it to prune entries.
    ///
    /// Write it as one conditional write (`INSERT … ON CONFLICT DO
    /// NOTHING`, Redis `SET NX`), never a read then a write.
    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool>;

    /// Rough count of tracked JTIs, for dashboards and tests. Not on
    /// the hot path.
    ///
    /// Returns `None` when the store cannot count cheaply, which is
    /// also the default, so an implementation may ignore this method.
    fn approx_size(&self) -> JtiFuture<'_, Option<usize>> {
        Box::pin(async { None })
    }
}

/// In-process JTI store. The default for every shipped JWT / handoff
/// consumer. Its entries are lost on restart and not shared with other
/// processes, so a revoked `jti` can still be replayed there.
pub struct InMemoryJtiStore {
    inner: Mutex<HashMap<String, i64>>,
}

impl InMemoryJtiStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Test-only entry count.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Default for InMemoryJtiStore {
    fn default() -> Self {
        Self::new()
    }
}

impl JtiStore for InMemoryJtiStore {
    // Bodies await nothing, so the `std::sync::Mutex` guard never
    // crosses a suspend point.
    fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            map.contains_key(jti)
        })
    }

    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Prune expired entries so memory stays bounded without a
            // background sweeper.
            let now = chrono::Utc::now().timestamp();
            map.retain(|_, &mut e| e > now);
            if map.contains_key(jti) {
                return false;
            }
            map.insert(jti.to_owned(), exp_unix);
            true
        })
    }

    fn approx_size(&self) -> JtiFuture<'_, Option<usize>> {
        Box::pin(async move { Some(self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_jti_is_not_used() {
        let store = InMemoryJtiStore::new();
        assert!(!store.is_used("token-1").await);
    }

    #[tokio::test]
    async fn first_mark_used_returns_true() {
        let store = InMemoryJtiStore::new();
        let exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("token-1", exp).await);
        assert!(store.is_used("token-1").await);
    }

    #[tokio::test]
    async fn second_mark_used_returns_false_single_use_guarantee() {
        let store = InMemoryJtiStore::new();
        let exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("token-1", exp).await);
        assert!(
            !store.mark_used("token-1", exp).await,
            "second use must return false to preserve single-use guard"
        );
    }

    #[tokio::test]
    async fn distinct_jtis_dont_collide() {
        let store = InMemoryJtiStore::new();
        let exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("token-a", exp).await);
        assert!(store.mark_used("token-b", exp).await);
        assert!(store.is_used("token-a").await);
        assert!(store.is_used("token-b").await);
        assert!(!store.is_used("token-c").await);
    }

    #[tokio::test]
    async fn expired_entries_are_pruned_on_next_mark() {
        let store = InMemoryJtiStore::new();
        let already_expired = chrono::Utc::now().timestamp() - 60;
        // Insert the expired entry through the lock: `mark_used`
        // would prune it right away.
        store
            .inner
            .lock()
            .unwrap()
            .insert("stale".to_owned(), already_expired);
        assert_eq!(store.len(), 1);
        let fresh_exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("fresh", fresh_exp).await);
        // `mark_used` pruned `stale`, so only `fresh` is left.
        assert_eq!(store.len(), 1);
        assert!(!store.is_used("stale").await);
        assert!(store.is_used("fresh").await);
    }

    #[tokio::test]
    async fn trait_object_is_usable() {
        // `Arc<dyn JtiStore>` is the shape every consumer takes, and
        // the reason the trait returns boxed futures.
        let store: std::sync::Arc<dyn JtiStore> = std::sync::Arc::new(InMemoryJtiStore::new());
        let exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("via-dyn", exp).await);
        assert!(store.is_used("via-dyn").await);
    }

    /// A store that awaits between the check and the record, like a
    /// database-backed one. `mark_used` must stay single-use under
    /// concurrency.
    #[tokio::test]
    async fn awaiting_store_keeps_the_single_use_guarantee() {
        struct AwaitingStore(Mutex<HashMap<String, i64>>);
        impl JtiStore for AwaitingStore {
            fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool> {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    self.0.lock().unwrap().contains_key(jti)
                })
            }
            fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    // Single conditional write, holding the lock across the
                    // check and the insert.
                    let mut map = self.0.lock().unwrap();
                    if map.contains_key(jti) {
                        return false;
                    }
                    map.insert(jti.to_owned(), exp_unix);
                    true
                })
            }
        }

        let store: std::sync::Arc<dyn JtiStore> =
            std::sync::Arc::new(AwaitingStore(Mutex::new(HashMap::new())));
        let exp = chrono::Utc::now().timestamp() + 60;

        // Two concurrent claims on the same jti: exactly one must win.
        let (a, b) = tokio::join!(store.mark_used("race", exp), store.mark_used("race", exp));
        assert!(a ^ b, "exactly one concurrent mark_used must succeed");
        assert!(store.is_used("race").await);
    }
}
