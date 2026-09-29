//! Issue #408 — file-system cache backend.
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
        .filter(|e| !e.file_name().to_string_lossy().starts_with(".lock-"))
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
    let count = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "cache"))
        .count();
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

/// Racing `add`s on one key: exactly one wins, and an expired entry frees it.
#[test]
fn concurrent_adds_have_exactly_one_winner() {
    let dir = unique_tmp_dir("add");
    for round in 0..40 {
        let key = format!("k{round}");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let wins: usize = (0..8)
            .map(|_| {
                let (dir, key, barrier) = (dir.clone(), key.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap();
                    barrier.wait();
                    rt.block_on(FileCache::new(dir).add(&key, "v", None))
                        .unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum();
        assert_eq!(wins, 1, "round {round}");
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        let cache = FileCache::new(&dir);
        assert!(cache
            .add("ttl", "a", Some(Duration::from_millis(20)))
            .await
            .unwrap());
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            cache.add("ttl", "b", None).await.unwrap(),
            "expired entry frees the key"
        );
        assert_eq!(cache.get("ttl").await.unwrap().as_deref(), Some("b"));
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `set` must replace the file atomically: a reader racing a rewrite sees
/// the old or the new value, never an empty or short one.
#[test]
fn concurrent_set_never_shows_a_torn_value() {
    let dir = unique_tmp_dir("torn");
    let value = "v".repeat(256 * 1024);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(FileCache::new(&dir).set("k", &value, None))
        .unwrap();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (dir, value, done) = (dir.clone(), value.clone(), done.clone());
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                let cache = FileCache::new(dir);
                let mut bad = 0usize;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if rt.block_on(cache.get("k")).unwrap().as_deref() != Some(value.as_str()) {
                        bad += 1;
                    }
                }
                bad
            })
        })
        .collect();
    let cache = FileCache::new(&dir);
    for _ in 0..300 {
        rt.block_on(cache.set("k", &value, None)).unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let bad: usize = readers.into_iter().map(|h| h.join().unwrap()).sum();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(bad, 0, "readers saw a missing or torn value");
}

/// Racing `add`s over an expired entry: exactly one wins. A reader that
/// saw the old entry must not delete the winner's fresh one.
#[test]
fn concurrent_adds_over_expired_entry_have_exactly_one_winner() {
    let dir = unique_tmp_dir("add-exp");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for round in 0..300 {
        let key = format!("k{round}");
        rt.block_on(FileCache::new(&dir).set(&key, "old", Some(Duration::from_millis(1))))
            .unwrap();
        std::thread::sleep(Duration::from_millis(3));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let wins: usize = (0..12)
            .map(|n| {
                let (dir, key, barrier) = (dir.clone(), key.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap();
                    let cache = FileCache::new(dir);
                    barrier.wait();
                    // Every third thread reads first, so a reader's clear races the adds.
                    if n % 3 == 0 {
                        let _ = rt.block_on(cache.get(&key)).unwrap();
                    }
                    rt.block_on(cache.add(&key, "v", None)).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum();
        assert_eq!(wins, 1, "round {round}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Racing `incr`s from many threads lose no count (a lockout counts with it).
#[test]
fn concurrent_incrs_lose_no_count() {
    let dir = unique_tmp_dir("incr");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (dir, barrier) = (dir.clone(), barrier.clone());
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                let cache = FileCache::new(dir);
                barrier.wait();
                for _ in 0..25 {
                    rt.block_on(cache.incr("n", 1, None)).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let n = rt.block_on(FileCache::new(&dir).get("n")).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(n.as_deref(), Some("200"));
}

/// `touch` keeps the value and moves the expiry.
#[tokio::test]
async fn touch_extends_a_live_entry() {
    let dir = unique_tmp_dir("touch");
    let cache = FileCache::new(&dir);
    cache
        .set("k", "v", Some(Duration::from_millis(30)))
        .await
        .unwrap();
    assert!(cache.touch("k", None).await.unwrap());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    assert!(!cache.touch("missing", None).await.unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `incr` with no TTL keeps a live counter's expiry, as `InMemoryCache` does.
#[tokio::test]
async fn incr_without_ttl_keeps_the_expiry() {
    let dir = unique_tmp_dir("incr-ttl");
    let cache = FileCache::new(&dir);
    cache
        .incr("n", 1, Some(Duration::from_millis(50)))
        .await
        .unwrap();
    assert_eq!(cache.incr("n", 1, None).await.unwrap(), 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let left = cache.get("n").await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(left, None, "the counter lost its expiry");
}
