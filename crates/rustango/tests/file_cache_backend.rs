//! Django-parity #408 — file-system cache backend.
//!
//! Verifies `FileCache` round-trips through the disk, applies TTL,
//! prunes expired entries on read, and is selectable through
//! `cache::from_settings` with `backend = "file"`.

#![cfg(all(feature = "cache", feature = "config"))]

use std::time::Duration;

use rustango::cache::{from_settings, Cache, FileCache};
use rustango::config::CacheSettings;

fn unique_tmp_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rustango-file-cache-{label}-{pid}-{nanos}"))
}

#[tokio::test]
async fn set_then_get_round_trips_through_disk() {
    let dir = unique_tmp_dir("rt");
    let cache = FileCache::new(&dir);
    cache.set("k", "hello", None).await.expect("set ok");
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("hello"));
    assert!(cache.exists("k").await.unwrap());
    assert!(dir.exists(), "set should auto-create the directory");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn delete_removes_the_file() {
    let dir = unique_tmp_dir("del");
    let cache = FileCache::new(&dir);
    cache.set("k", "v", None).await.unwrap();
    assert!(cache.exists("k").await.unwrap());
    cache.delete("k").await.unwrap();
    assert!(!cache.exists("k").await.unwrap());
    assert_eq!(cache.get("k").await.unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn ttl_expires_on_next_read() {
    let dir = unique_tmp_dir("ttl");
    let cache = FileCache::new(&dir);
    // 1s TTL — fast enough to test, slow enough not to race the
    // set itself.
    cache
        .set("k", "v", Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        cache.get("k").await.unwrap(),
        None,
        "TTL-expired entry should read as None",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn clear_removes_all_entries_but_keeps_dir() {
    let dir = unique_tmp_dir("clear");
    let cache = FileCache::new(&dir);
    cache.set("a", "1", None).await.unwrap();
    cache.set("b", "2", None).await.unwrap();
    cache.set("c", "3", None).await.unwrap();
    cache.clear().await.unwrap();
    assert_eq!(cache.get("a").await.unwrap(), None);
    assert_eq!(cache.get("b").await.unwrap(), None);
    assert_eq!(cache.get("c").await.unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn long_or_funny_keys_dont_break_filenames() {
    let dir = unique_tmp_dir("funny");
    let cache = FileCache::new(&dir);
    let funny = "user/../session/?id=1&token=*x*/with spaces and 🦀 unicode";
    cache.set(funny, "ok", None).await.unwrap();
    assert_eq!(cache.get(funny).await.unwrap().as_deref(), Some("ok"));
    // The on-disk filename must NOT contain the path separator.
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir exists")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(entries.len(), 1);
    let name = entries[0].file_name().to_string_lossy().into_owned();
    assert!(!name.contains('/'), "filename mustn't contain '/'");
    assert!(name.ends_with(".cache"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn from_settings_file_backend_round_trips() {
    let dir = unique_tmp_dir("settings");
    let s = CacheSettings {
        backend: Some("file".into()),
        file_cache_dir: Some(dir.clone()),
        ..Default::default()
    };
    let cache = from_settings(&s);
    cache.set("k", "v", None).await.unwrap();
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    // Confirm the on-disk side: a file landed under the configured dir.
    let count = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(count, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn from_settings_file_backend_without_dir_falls_back_to_memory() {
    let s = CacheSettings {
        backend: Some("file".into()),
        file_cache_dir: None,
        ..Default::default()
    };
    // No panic, no error — we fall back to InMemoryCache, which still
    // round-trips.
    let cache = from_settings(&s);
    cache.set("k", "v", None).await.unwrap();
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
}
