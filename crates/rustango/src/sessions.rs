//! Server-side session store backed by [`crate::cache::Cache`].
//!
//! The cookie holds only an opaque session ID. Everything else lives
//! in the cache. Use `RedisCache` when several replicas must share
//! sessions, or `InMemoryCache` for one process and for tests.
//!
//! Unlike a JWT, a session can be revoked: delete the cache entry and
//! every replica sees the user logged out. The cost is one cache read
//! per authenticated request.
//!
//! Send the cookie with `HttpOnly`, `Secure` and a `SameSite` value,
//! and call [`SessionStore::save`](crate::sessions::SessionStore::save)
//! again after login so the user gets
//! a new ID. Reusing the ID across the login boundary leaves the door
//! open to session fixation.
//!
//! [`SessionLayer`](crate::sessions::SessionLayer) sets and reads the cookie for you; handlers take
//! [`CurrentSession`](crate::sessions::CurrentSession). [`SessionStore::login`](crate::sessions::SessionStore::login) binds a session to a user,
//! so [`SessionStore::sessions_of`](crate::sessions::SessionStore::sessions_of) lists and `revoke_all` ends them.
//!
//! Under tenancy, scope the store per request with `SessionStore::for_tenant`:
//! session ids carry no tenant, and on a shared apex host one cookie reaches
//! every tenant.
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

mod layer;
pub use layer::{CurrentSession, SessionLayer};

const DEFAULT_TTL_SECS: u64 = 60 * 60 * 24 * 14; // 2 weeks
const ID_BYTES: usize = 24; // 192 bits, base64 → 32 chars
/// The session key holding who logged in; set only by [`SessionStore::login`].
const AUTH_KEY: &str = "_auth";
/// Most live sessions a user keeps; a login past it ends the oldest.
pub const MAX_SESSIONS_PER_USER: usize = 100;

/// Who a session belongs to and the user's revocation generation at login.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthMark {
    user: String,
    gen: i64,
    at: chrono::DateTime<chrono::Utc>,
}

/// One live session of a user, as [`SessionStore::sessions_of`] lists it.
/// `key` names it for [`SessionStore::revoke`]; it is not the session id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SessionInfo {
    pub key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A user's index entry: the id and when it was issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    id: String,
    at: chrono::DateTime<chrono::Utc>,
}

/// Per-request session bag: typed values under string keys, plus a
/// dirty flag so the store can skip a write when nothing changed.
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

    /// Read a typed value. `None` when the key is missing or the
    /// stored value is not a `T`.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.data
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Store a value and mark the session dirty.
    pub fn set<T: Serialize>(&mut self, key: impl Into<String>, value: T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.data.insert(key.into(), v);
            self.dirty = true;
        }
    }

    /// Remove a key and return its old value, if there was one.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let prev = self.data.remove(key);
        if prev.is_some() {
            self.dirty = true;
        }
        prev
    }

    /// Remove every key and mark the session dirty.
    pub fn clear(&mut self) {
        if !self.data.is_empty() {
            self.dirty = true;
        }
        self.data.clear();
    }

    /// `true` when something was set, removed or cleared since the
    /// last load or save.
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

    /// The user [`SessionStore::login`] bound this session to.
    #[must_use]
    pub fn user(&self) -> Option<String> {
        self.get::<AuthMark>(AUTH_KEY).map(|m| m.user)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    #[error("cache: {0}")]
    Cache(#[from] CacheError),
    #[error("session deserialize: {0}")]
    Serialization(String),
}

#[derive(Clone)]
pub struct SessionStore {
    cache: BoxedCache,
    space: crate::cache::keyspace::KeySpace,
    ttl: Arc<Duration>,
}

impl SessionStore {
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self {
            cache,
            space: crate::cache::keyspace::KeySpace::Global,
            ttl: Arc::new(Duration::from_secs(DEFAULT_TTL_SECS)),
        }
    }

    pub(crate) fn in_space(mut self, space: crate::cache::keyspace::KeySpace) -> Self {
        self.space = space;
        self
    }

    /// Keep sessions to one tenant: `tenant:#org:<id>:<slug>:session:<id>`.
    /// Unscoped, a session id saved under one tenant loads under every other.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn for_tenant(self, tenant: &crate::tenancy::TenantSlug) -> Self {
        self.in_space(crate::cache::keyspace::KeySpace::Tenant(tenant.clone()))
    }

    /// Set the per-session TTL. Default is 2 weeks.
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Arc::new(ttl);
        self
    }

    /// Store `session` under a new ID and return it. Use this at
    /// login, so the user gets a fresh ID. To update a session that
    /// already exists, use [`Self::save_with_id`].
    ///
    /// # Errors
    /// A cache or serialization error.
    pub async fn save(&self, session: &Session) -> Result<String, SessionError> {
        let id = generate_id();
        self.save_with_id(&id, session).await?;
        Ok(id)
    }

    /// Store `session` under `id`, replacing any existing entry.
    ///
    /// # Errors
    /// A cache or serialization error.
    pub async fn save_with_id(&self, id: &str, session: &Session) -> Result<(), SessionError> {
        let json = serde_json::to_string(session)
            .map_err(|e| SessionError::Serialization(e.to_string()))?;
        self.cache
            .set(&self.cache_key(id), &json, Some(*self.ttl))
            .await?;
        Ok(())
    }

    /// Load a session by ID. Missing, expired, unreadable and revoked
    /// entries all give `Ok(None)`.
    ///
    /// # Errors
    /// A cache error. A value that fails to parse gives `Ok(None)`
    /// instead, so a stored-format change does not 500 every request.
    pub async fn load(&self, id: &str) -> Result<Option<Session>, SessionError> {
        // The user index and generation keys share the area; never load them.
        if id.contains(':') {
            return Ok(None);
        }
        let Some(raw) = self.cache.get(&self.cache_key(id)).await? else {
            return Ok(None);
        };
        let mut session: Session = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        if let Some(mark) = session.get::<AuthMark>(AUTH_KEY) {
            if mark.gen != self.generation(&mark.user).await? {
                return Ok(None);
            }
        }
        // A freshly loaded session is clean until something changes it.
        session.dirty = false;
        Ok(Some(session))
    }

    /// Store `session` under a new id bound to `user`, so it shows in
    /// [`Self::sessions_of`] and dies with [`Self::revoke_all`].
    ///
    /// # Errors
    /// A cache or serialization error.
    pub async fn login(&self, user: &str, session: &Session) -> Result<String, SessionError> {
        Ok(self.login_session(user, session).await?.0)
    }

    /// [`Self::login`], plus the session as stored (clean, with its mark).
    pub(crate) async fn login_session(
        &self,
        user: &str,
        session: &Session,
    ) -> Result<(String, Session), SessionError> {
        let at = chrono::Utc::now();
        let mut session = session.clone();
        let gen = self.generation(user).await?;
        session.set(
            AUTH_KEY,
            AuthMark {
                user: user.to_owned(),
                gen,
                at,
            },
        );
        let id = self.save(&session).await?;
        let mut index = self.live_index(user).await?;
        index.push(IndexEntry { id: id.clone(), at });
        // Oldest first: a client that never logs out cannot grow it without end.
        let over = index.len().saturating_sub(MAX_SESSIONS_PER_USER);
        for old in index.drain(..over) {
            self.destroy(&old.id).await?;
        }
        self.write_index(user, &index).await?;
        session.dirty = false;
        Ok((id, session))
    }

    /// Before a request writes `session` back under `id`: take the stored
    /// login mark (a `revoke_all` that kept it moved its generation), or
    /// `false` when the stored session has ended, so it is never revived.
    pub(crate) async fn refresh_mark(
        &self,
        id: &str,
        session: &mut Session,
    ) -> Result<bool, SessionError> {
        let Some(stored) = self.load(id).await? else {
            return Ok(false);
        };
        if let Some(mark) = stored.data.get(AUTH_KEY) {
            session.data.insert(AUTH_KEY.to_owned(), mark.clone());
        }
        Ok(true)
    }

    /// Move `session` from `old` to a new id; a user's listing follows it.
    pub(crate) async fn reissue(
        &self,
        old: &str,
        session: &Session,
    ) -> Result<String, SessionError> {
        let id = self.save(session).await?;
        self.destroy(old).await?;
        if let Some(user) = session.user() {
            let mut index = self.read_index(&user).await?;
            if let Some(e) = index.iter_mut().find(|e| e.id == old) {
                e.id.clone_from(&id);
                self.write_index(&user, &index).await?;
            }
        }
        Ok(id)
    }

    pub(crate) fn ttl_value(&self) -> Duration {
        *self.ttl
    }

    /// The user's live sessions, oldest first. Under concurrent logins of
    /// one user an entry can be missed; [`Self::revoke_all`] still ends it.
    ///
    /// # Errors
    /// A cache error.
    pub async fn sessions_of(&self, user: &str) -> Result<Vec<SessionInfo>, SessionError> {
        let index = self.live_index(user).await?;
        self.write_index(user, &index).await?;
        Ok(index
            .iter()
            .map(|e| SessionInfo {
                key: public_key(&e.id),
                created_at: e.at,
            })
            .collect())
    }

    /// End the user's session that [`SessionInfo::key`] names. `false` when
    /// none of the user's live sessions has that key.
    ///
    /// # Errors
    /// A cache error.
    pub async fn revoke(&self, user: &str, key: &str) -> Result<bool, SessionError> {
        let mut index = self.live_index(user).await?;
        let Some(at) = index.iter().position(|e| public_key(&e.id) == key) else {
            return Ok(false);
        };
        let gone = index.remove(at);
        self.destroy(&gone.id).await?;
        self.write_index(user, &index).await?;
        Ok(true)
    }

    /// End every session of `user`, listed or not, except `keep` (a session
    /// id, such as the current one). Bumps the user's generation, so a
    /// session the index missed fails to load too.
    ///
    /// # Errors
    /// A cache or serialization error.
    pub async fn revoke_all(&self, user: &str, keep: Option<&str>) -> Result<(), SessionError> {
        let kept = match keep {
            Some(id) => self
                .load(id)
                .await?
                .filter(|s| s.user().as_deref() == Some(user)),
            None => None,
        };
        let gen = self
            .cache
            .incr(&self.user_key("gen", user), 1, None)
            .await?;
        for entry in self.read_index(user).await? {
            if Some(entry.id.as_str()) != keep {
                self.destroy(&entry.id).await?;
            }
        }
        let mut left = Vec::new();
        if let (Some(id), Some(mut session)) = (keep, kept) {
            if let Some(mut mark) = session.get::<AuthMark>(AUTH_KEY) {
                mark.gen = gen;
                let at = mark.at;
                session.set(AUTH_KEY, mark);
                self.save_with_id(id, &session).await?;
                left.push(IndexEntry {
                    id: id.to_owned(),
                    at,
                });
            }
        }
        self.write_index(user, &left).await
    }

    /// The user's revocation generation; 0 until the first `revoke_all`.
    async fn generation(&self, user: &str) -> Result<i64, SessionError> {
        Ok(self
            .cache
            .get(&self.user_key("gen", user))
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    async fn read_index(&self, user: &str) -> Result<Vec<IndexEntry>, SessionError> {
        let raw = self.cache.get(&self.user_key("user", user)).await?;
        Ok(raw
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default())
    }

    /// The index without entries whose session ended, was revoked or moved
    /// to another user. One generation read and one batch read.
    async fn live_index(&self, user: &str) -> Result<Vec<IndexEntry>, SessionError> {
        let index = self.read_index(user).await?;
        if index.is_empty() {
            return Ok(index);
        }
        let gen = self.generation(user).await?;
        let keys: Vec<String> = index.iter().map(|e| self.cache_key(&e.id)).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let found = self.cache.get_many(&refs).await?;
        Ok(index
            .into_iter()
            .zip(&keys)
            .filter(|(_, key)| {
                found
                    .get(*key)
                    .and_then(|raw| serde_json::from_str::<Session>(raw).ok())
                    .and_then(|s| s.get::<AuthMark>(AUTH_KEY))
                    .is_some_and(|m| m.user == user && m.gen == gen)
            })
            .map(|(e, _)| e)
            .collect())
    }

    async fn write_index(&self, user: &str, index: &[IndexEntry]) -> Result<(), SessionError> {
        let key = self.user_key("user", user);
        if index.is_empty() {
            self.cache.delete(&key).await?;
            return Ok(());
        }
        let json =
            serde_json::to_string(index).map_err(|e| SessionError::Serialization(e.to_string()))?;
        self.cache.set(&key, &json, Some(*self.ttl)).await?;
        Ok(())
    }

    /// `user:<user>` (index) and `gen:<user>`: a `:` keeps them apart from ids.
    fn user_key(&self, kind: &str, user: &str) -> String {
        self.cache_key(&format!("{kind}:{user}"))
    }

    /// Delete the session, as on logout. Does nothing for an unknown
    /// ID.
    ///
    /// # Errors
    /// A cache error.
    pub async fn destroy(&self, id: &str) -> Result<(), SessionError> {
        self.cache.delete(&self.cache_key(id)).await?;
        Ok(())
    }

    /// Extend the session's TTL without changing its contents. Call
    /// it on each request to keep active users signed in.
    ///
    /// # Errors
    /// A cache error. Returns `false` when the session is gone.
    pub async fn touch(&self, id: &str) -> Result<bool, SessionError> {
        // `Cache::touch` only extends a live key, so it cannot undo a racing destroy (#2300).
        Ok(self
            .cache
            .touch(&self.cache_key(id), Some(*self.ttl))
            .await?)
    }

    fn cache_key(&self, id: &str) -> String {
        self.space.key(crate::cache::keyspace::Area::Session, id)
    }
}

/// The shape [`generate_id`] makes: anything else in a cookie is not looked up.
fn is_session_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A session's name in listings: a hash, so a listing never leaks an id.
fn public_key(id: &str) -> String {
    use sha2::{Digest, Sha256};
    crate::hex::hex_encode(&Sha256::digest(id.as_bytes())[..12])
}

/// Make a 32-character base64url session ID with 192 bits of entropy,
/// above the 128 bits usually recommended. It draws from the OS CSPRNG
/// ([`rand::rngs::OsRng`]), not `thread_rng`, because a session ID is
/// an authentication secret.
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
        // A wrong-type read returns None instead of panicking.
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

    /// A cache where a logout lands inside every `get` and `touch`.
    struct LogoutMidTouch(InMemoryCache);

    #[async_trait::async_trait]
    impl crate::cache::Cache for LogoutMidTouch {
        async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
            let v = self.0.get(key).await?;
            self.0.delete(key).await?;
            Ok(v)
        }
        async fn set(&self, k: &str, v: &str, t: Option<Duration>) -> Result<(), CacheError> {
            self.0.set(k, v, t).await
        }
        async fn touch(&self, k: &str, t: Option<Duration>) -> Result<bool, CacheError> {
            self.0.delete(k).await?;
            self.0.touch(k, t).await
        }
        async fn delete(&self, key: &str) -> Result<(), CacheError> {
            self.0.delete(key).await
        }
        async fn exists(&self, key: &str) -> Result<bool, CacheError> {
            self.0.exists(key).await
        }
        async fn clear(&self) -> Result<(), CacheError> {
            self.0.clear().await
        }
    }

    /// #2300 — touch must not write back a session destroyed under it.
    #[tokio::test]
    async fn touch_does_not_revive_a_destroyed_session() {
        let inner = StdArc::new(LogoutMidTouch(InMemoryCache::new()));
        let store = SessionStore::new(inner.clone());
        let id = store.save(&Session::new()).await.unwrap();
        assert!(!store.touch(&id).await.unwrap());
        assert!(store.load(&id).await.unwrap().is_none(), "session revived");
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
        // Change it and save under the same id.
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
        // load() must not panic; it returns None.
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

    /// Unscoped keys stay `session:<id>`; a tenant's carry its org id and slug.
    #[tokio::test]
    async fn session_keys_are_bare_or_bound_to_the_tenant() {
        let root: BoxedCache = StdArc::new(InMemoryCache::new());
        let id = SessionStore::new(root.clone())
            .save(&Session::new())
            .await
            .unwrap();
        assert!(root.exists(&format!("session:{id}")).await.unwrap());
        #[cfg(feature = "tenancy")]
        {
            use crate::tenancy::TenantSlug;
            let acme = SessionStore::new(root.clone()).for_tenant(&TenantSlug::new(1, "acme"));
            let heir = SessionStore::new(root.clone()).for_tenant(&TenantSlug::new(2, "acme"));
            let id = acme.save(&Session::new()).await.unwrap();
            let key = format!("tenant:#org:1:acme:session:{id}");
            assert!(root.exists(&key).await.unwrap());
            assert!(
                heir.load(&id).await.unwrap().is_none(),
                "slug reuse loads it"
            );
            assert!(SessionStore::new(root).load(&id).await.unwrap().is_none());
        }
    }

    /// Logins list per user by a key that is not the id; revoke ends one.
    #[tokio::test]
    async fn login_lists_and_revokes_per_user() {
        let store = store();
        let a = store.login("u1", &Session::new()).await.unwrap();
        let b = store.login("u1", &Session::new()).await.unwrap();
        let other = store.login("u2", &Session::new()).await.unwrap();
        let listed = store.sessions_of("u1").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|i| i.key != a && i.key != b));
        let u2_key = store.sessions_of("u2").await.unwrap()[0].key.clone();
        assert!(
            !store.revoke("u1", &u2_key).await.unwrap(),
            "revoked another user's"
        );
        assert!(store.revoke("u1", &listed[0].key).await.unwrap());
        assert!(store.load(&a).await.unwrap().is_none());
        assert!(store.load(&b).await.unwrap().is_some());
        assert!(store.load(&other).await.unwrap().is_some());
        assert_eq!(store.sessions_of("u1").await.unwrap().len(), 1);
        assert_eq!(
            store.load(&b).await.unwrap().unwrap().user().as_deref(),
            Some("u1")
        );
    }

    /// `revoke_all` ends a session the index lost, and keeps the named one.
    #[tokio::test]
    async fn revoke_all_ends_unlisted_sessions_and_keeps_one() {
        let store = store();
        let keep = store.login("u", &Session::new()).await.unwrap();
        let lost = store.login("u", &Session::new()).await.unwrap();
        store
            .cache
            .delete(&store.user_key("user", "u"))
            .await
            .unwrap();
        let listed = store.login("u", &Session::new()).await.unwrap();
        store.revoke_all("u", Some(&keep)).await.unwrap();
        assert!(
            store.load(&lost).await.unwrap().is_none(),
            "unlisted survived"
        );
        assert!(store.load(&listed).await.unwrap().is_none());
        assert!(store.load(&keep).await.unwrap().is_some());
        assert_eq!(store.sessions_of("u").await.unwrap().len(), 1);
        // A later login is live again.
        let next = store.login("u", &Session::new()).await.unwrap();
        assert!(store.load(&next).await.unwrap().is_some());
    }

    /// A cookie naming the index or generation key loads nothing.
    #[tokio::test]
    async fn index_keys_never_load_as_sessions() {
        let store = store();
        store.login("u", &Session::new()).await.unwrap();
        store.revoke_all("u", None).await.unwrap();
        assert!(store.load("user:u").await.unwrap().is_none());
        assert!(store.load("gen:u").await.unwrap().is_none());
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
