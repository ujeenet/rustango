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
//!   `mark_used` pops expired entries off an expiry heap, so memory
//!   stays bounded with no sweeper and no full-map scan.
//! - Anything else — implement [`JtiStore`] over Redis, a database
//!   table, or another shared store.
//!   [`RedisCache`](crate::cache::redis_backend::RedisCache) is the
//!   smallest bridge.
//!
//! [`InMemoryJtiStore`]: crate::jti_store::InMemoryJtiStore
//! [`JtiStore`]: crate::jti_store::JtiStore

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

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
///         // One atomic round trip. Not `rows_affected == 1`: a MySQL
///         // skip also reports 1 row, so a replay would pass.
///         rustango::sql::insert_or_ignore(&pool, &insert).await.unwrap_or(false)
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
    /// Write it as one conditional write
    /// ([`insert_or_ignore`](crate::sql::insert_or_ignore), Redis `SET NX`),
    /// never a read then a write.
    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool>;

    /// Rough count of tracked JTIs, for dashboards and tests. Not on
    /// the hot path.
    ///
    /// Returns `None` when the store cannot count cheaply, which is
    /// also the default, so an implementation may ignore this method.
    fn approx_size(&self) -> JtiFuture<'_, Option<usize>> {
        Box::pin(async { None })
    }

    /// `true` when revocations live in this process only; other
    /// replicas keep accepting a revoked token.
    fn is_process_local(&self) -> bool {
        false
    }
}

/// In-process JTI store. The default for every shipped JWT / handoff
/// consumer. Its entries are lost on restart and not shared with other
/// processes, so a revoked `jti` can still be replayed there.
pub struct InMemoryJtiStore {
    inner: Mutex<JtiMap>,
}

/// `jti -> exp` plus a min-heap on `exp`, so a prune pops only the
/// expired entries instead of scanning the whole map (#2514).
#[derive(Default)]
struct JtiMap {
    map: HashMap<Arc<str>, i64>,
    expiry: BinaryHeap<Reverse<(i64, Arc<str>)>>,
}

impl JtiMap {
    fn prune(&mut self, now: i64) {
        while let Some(Reverse((exp, _))) = self.expiry.peek() {
            if *exp > now {
                break;
            }
            let Some(Reverse((exp, jti))) = self.expiry.pop() else {
                break;
            };
            // Only drop the entry this heap node was pushed for.
            if self.map.get(&jti) == Some(&exp) {
                self.map.remove(&jti);
            }
        }
    }

    fn insert(&mut self, jti: &str, exp: i64) {
        let key: Arc<str> = Arc::from(jti);
        self.map.insert(Arc::clone(&key), exp);
        self.expiry.push(Reverse((exp, key)));
    }
}

impl InMemoryJtiStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(JtiMap::default()),
        }
    }

    /// Test-only entry count.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }
}

impl Default for InMemoryJtiStore {
    fn default() -> Self {
        Self::new()
    }
}

impl JtiStore for InMemoryJtiStore {
    fn is_process_local(&self) -> bool {
        true
    }

    // Bodies await nothing, so the `std::sync::Mutex` guard never
    // crosses a suspend point.
    fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.map.contains_key(jti)
        })
    }

    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.prune(chrono::Utc::now().timestamp());
            if inner.map.contains_key(jti) {
                return false;
            }
            inner.insert(jti, exp_unix);
            true
        })
    }

    fn approx_size(&self) -> JtiFuture<'_, Option<usize>> {
        Box::pin(async move {
            Some(
                self.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .map
                    .len(),
            )
        })
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
        store.inner.lock().unwrap().insert("stale", already_expired);
        assert_eq!(store.len(), 1);
        let fresh_exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("fresh", fresh_exp).await);
        // `mark_used` pruned `stale`, so only `fresh` is left.
        assert_eq!(store.len(), 1);
        assert!(!store.is_used("stale").await);
        assert!(store.is_used("fresh").await);
    }

    /// A stale heap node for a re-used jti must not drop the live entry.
    #[test]
    fn prune_pops_only_expired_and_keeps_reinserted_jti() {
        let mut m = JtiMap::default();
        m.insert("a", 10);
        m.insert("b", 100);
        m.prune(50);
        assert!(!m.map.contains_key("a"));
        assert!(m.map.contains_key("b"));
        // Heap node for `b` stays; nothing else was scanned.
        assert_eq!(m.expiry.len(), 1);
        m.insert("a", 200);
        m.insert("c", 20);
        m.map.insert(Arc::from("c"), 300); // live exp moved past the old node
        m.prune(150);
        assert!(m.map.contains_key("a"));
        assert!(
            m.map.contains_key("c"),
            "stale heap node removed a live entry"
        );
        assert!(!m.map.contains_key("b"));
    }

    #[test]
    fn only_the_in_memory_store_is_process_local() {
        struct Shared;
        impl JtiStore for Shared {
            fn is_used<'a>(&'a self, _: &'a str) -> JtiFuture<'a, bool> {
                Box::pin(async { false })
            }
            fn mark_used<'a>(&'a self, _: &'a str, _: i64) -> JtiFuture<'a, bool> {
                Box::pin(async { true })
            }
        }
        assert!(InMemoryJtiStore::new().is_process_local());
        assert!(!Shared.is_process_local());
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
