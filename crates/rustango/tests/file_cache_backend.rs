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

/// #1233 — an entry must be readable for the *whole* TTL it was
/// promised, including immediately after the write.
///
/// The old encoding stamped `expires_at` in whole seconds and expired on
/// `now >= expires_at`, so a `set` landing at wall-clock `T.999` was
/// already expired by the read a millisecond later. This deliberately
/// starts each iteration just before a second boundary, which is the
/// window that made the failure load-dependent rather than impossible.
#[tokio::test]
async fn entry_is_readable_immediately_even_across_a_second_boundary() {
    let dir = unique_tmp_dir("boundary");
    let cache = FileCache::new(&dir);

    for i in 0..5 {
        // Sleep to within ~5ms of the next whole second.
        let sub_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_millis()))
            .unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(995u64.saturating_sub(sub_ms))).await;

        let key = format!("boundary-{i}");
        cache
            .set(&key, "v", Some(Duration::from_secs(1)))
            .await
            .unwrap();
        assert_eq!(
            cache.get(&key).await.unwrap().as_deref(),
            Some("v"),
            "entry {i} expired immediately after being written",
        );
    }
}

/// #1233 — sub-second TTLs were unrepresentable: `as_secs()` truncated
/// them to `0`, so `expires_at == now` and the entry was born expired.
#[tokio::test]
async fn sub_second_ttl_is_honored_not_truncated_to_zero() {
    let dir = unique_tmp_dir("subsec");
    let cache = FileCache::new(&dir);

    cache
        .set("k", "v", Some(Duration::from_millis(500)))
        .await
        .unwrap();
    assert_eq!(
        cache.get("k").await.unwrap().as_deref(),
        Some("v"),
        "a 500ms entry must exist immediately after the write",
    );

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        cache.get("k").await.unwrap(),
        None,
        "a 500ms entry must be gone after 700ms",
    );
}

/// Parity guard: `InMemoryCache` already handled both cases (it stores an
/// `Instant`). Pinning them side by side stops the two backends drifting
/// apart on the same public API again.
#[tokio::test]
async fn memory_backend_agrees_on_sub_second_ttl() {
    let cache = rustango::cache::InMemoryCache::new();

    cache
        .set("k", "v", Some(Duration::from_millis(500)))
        .await
        .unwrap();
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(cache.get("k").await.unwrap(), None);
}
