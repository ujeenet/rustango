//! Server-side session store backed by [`crate::cache::Cache`].
//!
//! The cookie carries only an opaque session ID; everything else
//! lives in the cache. Pair with `RedisCache` for cross-replica
//! visibility, or `InMemoryCache` for single-process / tests.
//!
//! Different shape from JWT: sessions are revocable on the server
//! (delete the cache entry → all replicas see it), at the cost of
//! a cache lookup per authenticated request. Pick JWT for stateless
//! auth, sessions for "log this user out NOW" semantics.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::sessions::{Session, SessionStore};
//! use rustango::cache::{BoxedCache, InMemoryCache};
//! use std::sync::Arc;
//!
//! let store = SessionStore::new(redis_cache);
//!
//! // After successful login:
//! let mut session = Session::new();
//! session.set("user_id", 42);
//! session.set("csrf_at", chrono::Utc::now().to_rfc3339());
//! let id = store.save(&session).await?;
//! // Set a cookie: format!("rustango_session={id}; HttpOnly; SameSite=Lax")
//!
//! // On subsequent requests:
//! let session = store.load(&id).await?.unwrap_or_default();
//! let user_id: Option<i64> = session.get("user_id");
//!
//! // Logout — drops the cache entry, cookie is now meaningless.
//! store.destroy(&id).await;
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use crate::cache::{BoxedCache, CacheError};

const KEY_PREFIX: &str = "session";
const DEFAULT_TTL_SECS: u64 = 60 * 60 * 24 * 14; // 2 weeks
const ID_BYTES: usize = 24; // 192 bits, base64 → 32 chars

/// Per-request session bag. Holds typed values keyed by string, plus
/// a dirty-bit so the store can skip a write when nothing changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    data: HashMap<String, Value>,
    #[serde(skip)]
    dirty: bool,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a typed value. Returns `None` when absent or when the
    /// stored shape doesn't deserialize as `T`.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.data
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Store a value, marking the session dirty.
    pub fn set<T: Serialize>(&mut self, key: impl Into<String>, value: T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.data.insert(key.into(), v);
            self.dirty = true;
        }
    }

    /// Remove a key; returns the previous value if any.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let prev = self.data.remove(key);
        if prev.is_some() {
            self.dirty = true;
        }
        prev
    }

    /// Wipe every key. Marks dirty.
    pub fn clear(&mut self) {
        if !self.data.is_empty() {
            self.dirty = true;
        }
        self.data.clear();
    }

    /// `true` when the in-memory state diverges from what's in the
    /// cache (anything was set / removed / cleared since the last
    /// load or save).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[must_use]
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.data.keys()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("cache: {0}")]
    Cache(#[from] CacheError),
    #[error("session deserialize: {0}")]
    Serialization(String),
}

#[derive(Clone)]
pub struct SessionStore {
    cache: BoxedCache,
    ttl: Arc<Duration>,
}

impl SessionStore {
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self {
            cache,
            ttl: Arc::new(Duration::from_secs(DEFAULT_TTL_SECS)),
        }
    }

    /// Override the per-session TTL. Default 2 weeks.
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Arc::new(ttl);
        self
    }

    /// Persist `session` and return its ID. Always generates a fresh
    /// ID — call [`Self::save_with_id`] to update an existing session
    /// in place (typical request-cycle pattern).
    ///
    /// # Errors
    /// Underlying cache or serialization error.
    pub async fn save(&self, session: &Session) -> Result<String, SessionError> {
        let id = generate_id();
        self.save_with_id(&id, session).await?;
        Ok(id)
    }

    /// Persist `session` under the given `id` (rewriting any existing
    /// entry).
    ///
    /// # Errors
    /// Underlying cache or serialization error.
    pub async fn save_with_id(&self, id: &str, session: &Session) -> Result<(), SessionError> {
        let json = serde_json::to_string(session)
            .map_err(|e| SessionError::Serialization(e.to_string()))?;
        self.cache
            .set(&self.cache_key(id), &json, Some(*self.ttl))
            .await?;
        Ok(())
    }

    /// Load by ID. Returns `Ok(None)` for absent / expired / corrupted
    /// (we treat corrupt as absent to fail-open).
    ///
    /// # Errors
    /// Underlying cache error. Deserialization errors are demoted to
    /// `Ok(None)` so a cache schema change doesn't 500 every request.
    pub async fn load(&self, id: &str) -> Result<Option<Session>, SessionError> {
        let Some(raw) = self.cache.get(&self.cache_key(id)).await? else {
            return Ok(None);
        };
        let mut session: Session = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        // Loaded session starts clean — only later modifications mark dirty.
        session.dirty = false;
        Ok(Some(session))
    }

    /// Destroy the session — typical for logout. No-op if the ID
    /// is unknown.
    ///
    /// # Errors
    /// Underlying cache error.
    pub async fn destroy(&self, id: &str) -> Result<(), SessionError> {
        self.cache.delete(&self.cache_key(id)).await?;
        Ok(())
    }

    /// Refresh the session's TTL without rewriting its contents.
    /// Common pattern: call on every request to keep active users
    /// signed in (sliding expiration).
    ///
    /// # Errors
    /// Underlying cache error. No-op when the session doesn't exist.
    pub async fn touch(&self, id: &str) -> Result<bool, SessionError> {
        let key = self.cache_key(id);
        let Some(raw) = self.cache.get(&key).await? else {
            return Ok(false);
        };
        self.cache.set(&key, &raw, Some(*self.ttl)).await?;
        Ok(true)
    }

    fn cache_key(&self, id: &str) -> String {
        format!("{KEY_PREFIX}:{id}")
    }
}

/// Generate a 32-character base64url session ID. 192 bits of entropy
/// — comfortably more than the standards-recommended 128 for session
/// tokens. Sources from [`rand::rngs::OsRng`] (the OS CSPRNG) rather
/// than `thread_rng`; session IDs are auth-boundary material and want
/// the strongest available source. v0.42.
fn generate_id() -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use rand::{rngs::OsRng, RngCore};
    let mut buf = [0u8; ID_BYTES];
    OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;
    use std::sync::Arc as StdArc;

    fn store() -> SessionStore {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        SessionStore::new(cache)
    }

    // -------- Session bag

    #[test]
    fn fresh_session_is_clean_and_empty() {
        let s = Session::new();
        assert!(!s.is_dirty());
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn set_marks_dirty_and_stores() {
        let mut s = Session::new();
        s.set("user_id", 42_i64);
        assert!(s.is_dirty());
        assert_eq!(s.get::<i64>("user_id"), Some(42));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn get_returns_none_for_missing() {
        let s = Session::new();
        assert_eq!(s.get::<i64>("nope"), None);
    }

    #[test]
    fn get_returns_none_for_wrong_type() {
        let mut s = Session::new();
        s.set("flag", "string-not-a-number");
        // Cross-type read returns None instead of panicking.
        assert_eq!(s.get::<i64>("flag"), None);
    }

    #[test]
    fn remove_returns_previous_and_marks_dirty() {
        let mut s = Session::new();
        s.set("k", "v");
        let prev = s.remove("k");
        assert_eq!(prev.unwrap(), "v");
        assert!(s.is_dirty());
        assert!(s.is_empty());
    }

    #[test]
    fn remove_missing_does_not_mark_dirty() {
        let mut s = Session::new();
        assert!(s.remove("nope").is_none());
        assert!(!s.is_dirty());
    }

    #[test]
    fn clear_wipes_all_keys() {
        let mut s = Session::new();
        s.set("a", 1);
        s.set("b", 2);
        s.clear();
        assert!(s.is_empty());
        assert!(s.is_dirty());
    }

    #[test]
    fn keys_iterates_inserted_keys() {
        let mut s = Session::new();
        s.set("a", 1);
        s.set("b", 2);
        let mut keys: Vec<&String> = s.keys().collect();
        keys.sort();
        assert_eq!(
            keys.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    // -------- SessionStore

    #[tokio::test]
    async fn save_then_load_roundtrips() {
        let store = store();
        let mut s = Session::new();
        s.set("user_id", 42_i64);
        s.set("name", "Alice");
        let id = store.save(&s).await.unwrap();

        let loaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.get::<i64>("user_id"), Some(42));
        assert_eq!(loaded.get::<String>("name").as_deref(), Some("Alice"));
        // Loaded session starts clean.
        assert!(!loaded.is_dirty());
    }

    #[tokio::test]
    async fn load_unknown_id_returns_none() {
        let store = store();
        assert!(store.load("does-not-exist").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn destroy_removes_session() {
        let store = store();
        let id = store.save(&Session::new()).await.unwrap();
        assert!(store.load(&id).await.unwrap().is_some());
        store.destroy(&id).await.unwrap();
        assert!(store.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn touch_extends_ttl_on_existing_session() {
        let store = store();
        let id = store.save(&Session::new()).await.unwrap();
        assert!(store.touch(&id).await.unwrap());
        assert!(store.load(&id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn touch_returns_false_on_missing_session() {
        let store = store();
        assert!(!store.touch("does-not-exist").await.unwrap());
    }

    #[tokio::test]
    async fn save_with_id_rewrites_existing_session() {
        let store = store();
        let mut s = Session::new();
        s.set("v", 1);
        let id = store.save(&s).await.unwrap();
        // Mutate + save in place
        let mut loaded = store.load(&id).await.unwrap().unwrap();
        loaded.set("v", 2);
        store.save_with_id(&id, &loaded).await.unwrap();
        let again = store.load(&id).await.unwrap().unwrap();
        assert_eq!(again.get::<i64>("v"), Some(2));
    }

    #[tokio::test]
    async fn each_save_generates_distinct_id() {
        let store = store();
        let id1 = store.save(&Session::new()).await.unwrap();
        let id2 = store.save(&Session::new()).await.unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn corrupted_cache_value_loads_as_none() {
        let store = store();
        // Plant garbage under a session key.
        store
            .cache
            .set(
                "session:corrupt",
                "not-json-{}",
                Some(Duration::from_secs(60)),
            )
            .await
            .unwrap();
        // load() should NOT panic; returns None.
        assert!(store.load("corrupt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn complex_value_roundtrips() {
        let store = store();
        let mut s = Session::new();
        let payload = serde_json::json!({"role": "admin", "perms": ["read", "write"]});
        s.set("ctx", payload.clone());
        let id = store.save(&s).await.unwrap();
        let loaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.get::<serde_json::Value>("ctx"), Some(payload));
    }

    #[test]
    fn generated_id_is_url_safe_and_192_bits() {
        let id = generate_id();
        // 24 bytes encoded = ceil(24*4/3) = 32 chars (no padding).
        assert_eq!(id.len(), 32);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn generated_ids_are_distinct() {
        let a = generate_id();
        let b = generate_id();
        assert_ne!(a, b);
    }
}
