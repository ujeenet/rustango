//! Django-parity #394 — file-based session backend.
//!
//! Rustango's `SessionStore` is a thin wrapper over the `Cache`
//! trait, so any `Cache` implementation becomes a session backend
//! for free. This test pins that composition for the file-system
//! backend (#408): a `SessionStore::new(Arc::new(FileCache::new(dir)))`
//! round-trips through disk, survives a fresh `SessionStore`
//! instance built on the same directory (process-restart durability),
//! and respects `destroy()`.

#![cfg(all(feature = "sessions", feature = "cache"))]

use std::sync::Arc;

use rustango::cache::{BoxedCache, FileCache};
use rustango::sessions::{Session, SessionStore};

fn unique_tmp_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rustango-file-session-{label}-{pid}-{nanos}"))
}

fn file_store(dir: &std::path::Path) -> SessionStore {
    let cache: BoxedCache = Arc::new(FileCache::new(dir));
    SessionStore::new(cache)
}

#[tokio::test]
async fn save_then_load_round_trips_through_disk() {
    let dir = unique_tmp_dir("rt");
    let store = file_store(&dir);

    let mut s = Session::new();
    s.set("user_id", 42_i64);
    s.set("flash", "welcome");

    let id = store.save(&s).await.expect("save ok");
    let loaded = store
        .load(&id)
        .await
        .expect("load ok")
        .expect("session should exist");
    assert_eq!(loaded.get::<i64>("user_id"), Some(42));
    assert_eq!(loaded.get::<String>("flash"), Some("welcome".into()));
    assert!(!loaded.is_dirty(), "freshly-loaded session is clean");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn session_survives_fresh_store_on_same_dir() {
    // The whole point of file-backed sessions is process-restart
    // durability — a new SessionStore on the same dir must see
    // sessions previously written by another instance.
    let dir = unique_tmp_dir("survive");
    let id = {
        let store = file_store(&dir);
        let mut s = Session::new();
        s.set("flag", true);
        store.save(&s).await.expect("save ok")
    };

    // Drop the first store + cache, build a brand-new pair.
    let store2 = file_store(&dir);
    let loaded = store2
        .load(&id)
        .await
        .expect("load ok")
        .expect("session should survive into the second store");
    assert_eq!(loaded.get::<bool>("flag"), Some(true));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn destroy_removes_session_from_disk() {
    let dir = unique_tmp_dir("destroy");
    let store = file_store(&dir);

    let mut s = Session::new();
    s.set("k", "v");
    let id = store.save(&s).await.expect("save ok");
    assert!(store.load(&id).await.unwrap().is_some());
    store.destroy(&id).await.expect("destroy ok");
    assert!(
        store.load(&id).await.unwrap().is_none(),
        "destroyed session should be gone"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unknown_id_loads_as_none() {
    let dir = unique_tmp_dir("missing");
    let store = file_store(&dir);
    let loaded = store
        .load("does-not-exist-id")
        .await
        .expect("load ok — missing returns Ok(None)");
    assert!(loaded.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
