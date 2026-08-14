//! Pluggable JTI (JWT ID) revocation / single-use store.
//!
//! A `JtiStore` records which JWT identifiers have been "spent"
//! (either revoked via logout, or single-use-consumed by an
//! impersonation handoff redemption) so the next request bearing the
//! same `jti` is rejected.
//!
//! Two impls ship out of the box; users can implement the trait
//! against any backing store:
//!
//! - [`InMemoryJtiStore`] — process-local `Mutex<HashMap<jti, exp>>`.
//!   Correct for single-instance dev and tests. The default in every
//!   shipped consumer. Memory stays bounded because every entry's
//!   `exp` is within the token's TTL window of insertion;
//!   `mark_used` prunes expired entries opportunistically on every
//!   call so there's no background sweeper to run.
//! - Roll your own — implement [`JtiStore`] against Redis, a
//!   database table, etcd, or whatever multi-instance store fits
//!   your deployment. A Redis-backed impl wired through
//!   [`crate::cache::RedisCache`] is the smallest possible bridge.
//!
//! ## Why a trait?
//!
//! Pre-v0.47 the framework hard-coded an in-memory map for both
//! [`crate::tenancy::jwt_lifecycle::JwtLifecycle`] and the
//! impersonation handoff JTI blacklist. In a horizontally-scaled
//! deployment that meant a revoked refresh token could be replayed
//! against a different process within the token's TTL window — the
//! audit-flagged "multi-instance JWT replay" gap. Extracting the
//! trait lets operators plug in a shared store without forking
//! rustango.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

/// Future returned by the [`JtiStore`] methods.
///
/// Boxed rather than an `async fn` in the trait because every consumer
/// holds the store as `Arc<dyn JtiStore>`, and native `async fn` in traits
/// is not dyn-compatible. This is exactly what `#[async_trait]` expands to;
/// it's written out by hand so `jti_store` — a core, ungated module — keeps
/// building under every feature combination, including
/// `--no-default-features --features sqlite,tenancy`, where the optional
/// `async-trait` dependency isn't compiled in.
pub type JtiFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Storage backend for "this JTI is no longer valid" lookups.
///
/// Implementations MUST make `mark_used` atomic — concurrent callers
/// for the same `jti` must observe exactly one success (the rest
/// must observe `false`), or the single-use guarantee is broken.
///
/// # Async since v0.52 (#1191)
///
/// The methods return futures so a store can simply `await` a database
/// write. While the trait was synchronous, a durable store had to be a hot
/// `RwLock<HashMap>` with a background flusher — eventually consistent, with
/// a convergence window during which a revoked `jti` was still accepted on
/// another instance. Revoking a token is precisely the operation you want to
/// be durable and immediate, and that constraint came from this signature
/// rather than from the problem.
///
/// A synchronous implementation stays trivial — wrap the body:
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
/// …and a durable one is now just a query:
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

    /// Atomically check + record. Returns `true` when the JTI was
    /// newly recorded (i.e. the caller is the first to "use" it);
    /// returns `false` when the JTI was already present (replay).
    ///
    /// `exp_unix` is the JWT's `exp` claim (unix seconds). Stores
    /// MAY use it to prune entries that are no longer relevant.
    ///
    /// The atomicity requirement is unchanged by being async: a durable
    /// store should express it as a single conditional write (e.g.
    /// `INSERT … ON CONFLICT DO NOTHING`, or Redis `SET NX`), not as a
    /// read followed by a write.
    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool>;

    /// Approximate count of currently-tracked JTIs. Used by admin
    /// dashboards and tests; not on the hot path.
    ///
    /// Returns `None` when the backing store can't cheaply count
    /// (e.g. Redis with millions of entries — calling `SCAN` for
    /// a status display would be silly). Default impl returns
    /// `None` so trait objects don't have to know how to count.
    /// v0.48.
    fn approx_size(&self) -> JtiFuture<'_, Option<usize>> {
        Box::pin(async { None })
    }
}

/// In-process JTI store. Backs the default behaviour for every
/// shipped JWT / handoff consumer.
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

    /// Test-only accessor for the current entry count. Useful for
    /// asserting `mark_used` actually inserts.
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
    // Bodies stay synchronous — nothing is awaited, so the `std::sync::Mutex`
    // guard never crosses a suspend point.
    fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            map.contains_key(jti)
        })
    }

    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Prune expired entries opportunistically so memory stays
            // bounded without a background sweeper.
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
        // Pre-poison the store with an expired entry — we can't go
        // through `mark_used` for that since `mark_used` would prune
        // the entry it just inserted. Grab the lock directly.
        store
            .inner
            .lock()
            .unwrap()
            .insert("stale".to_owned(), already_expired);
        assert_eq!(store.len(), 1);
        let fresh_exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("fresh", fresh_exp).await);
        // Pruning ran during `mark_used("fresh", …)` and removed
        // the expired `stale` entry. Now the map holds only `fresh`.
        assert_eq!(store.len(), 1);
        assert!(!store.is_used("stale").await);
        assert!(store.is_used("fresh").await);
    }

    #[tokio::test]
    async fn trait_object_is_usable() {
        // Lock in the dyn-compatible contract — `Arc<dyn JtiStore>`
        // is the shape every consumer takes, and the reason the trait
        // returns boxed futures rather than using `async fn` (#1191).
        let store: std::sync::Arc<dyn JtiStore> = std::sync::Arc::new(InMemoryJtiStore::new());
        let exp = chrono::Utc::now().timestamp() + 60;
        assert!(store.mark_used("via-dyn", exp).await);
        assert!(store.is_used("via-dyn").await);
    }

    /// A store that actually awaits between the check and the record — the
    /// shape a database-backed store has. `mark_used` must still be
    /// single-use under concurrency, which is why the contract demands one
    /// conditional write rather than read-then-write (#1191).
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
