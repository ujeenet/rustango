//! Backing test for `docs/auth-sessions.md`. Uses an in-memory cache — no
//! Redis or database needed. In production you pass a `RedisCache` so every
//! replica sees the same sessions (and a logout on one is seen by all).
//!
//! Run: `cargo test -p auth_demo --test auth_sessions`

use std::sync::Arc;
use std::time::Duration;

use rustango::cache::{BoxedCache, InMemoryCache};
use rustango::sessions::{Session, SessionStore};

fn store() -> SessionStore {
    let cache: BoxedCache = Arc::new(InMemoryCache::new());
    SessionStore::new(cache).ttl(Duration::from_secs(60 * 60))
}

#[tokio::test]
async fn login_saves_a_session_and_the_id_loads_it_back() {
    let store = store();

    // After the password check, stash who the user is and save → opaque id.
    let mut session = Session::new();
    session.set("user_id", 42_i64);
    let sid = store.save(&session).await.unwrap();

    // The cookie carries only `sid`; the data lives server-side in the cache.
    let loaded = store.load(&sid).await.unwrap().expect("session present");
    assert_eq!(loaded.get::<i64>("user_id"), Some(42));
}

#[tokio::test]
async fn logout_destroys_the_session_immediately() {
    let store = store();
    let sid = store.save(&Session::new()).await.unwrap();
    assert!(store.load(&sid).await.unwrap().is_some());

    store.destroy(&sid).await.unwrap();
    // Revocable on the server: the cookie is now meaningless on every replica.
    assert!(store.load(&sid).await.unwrap().is_none());
}

#[tokio::test]
async fn unknown_or_tampered_id_loads_as_none() {
    let store = store();
    assert!(store.load("not-a-real-session-id").await.unwrap().is_none());
}
