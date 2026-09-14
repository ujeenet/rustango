//! Backing test for `docs/caching.md` — `ScopedCache::clear()` on a
//! `FileCache` must not reach another tenant's entries.
//!
//! `ScopedCache::clear()` calls `inner.delete_prefix(prefix)`. `FileCache`
//! did not implement that, so it fell through to the trait default, which
//! called `clear()` — and `FileCache::clear()` removes every `.cache` file
//! in the directory. One tenant's clear wiped every tenant's entries,
//! while the multi-tenancy section of the page said it dropped "ONLY
//! acme's entries".
//!
//! It was not an oversight so much as a dead end: filenames are SHA-256 of
//! the key, so nothing about the original key survived to disk and a
//! prefix delete was impossible to implement. The fix stores the key in
//! the file.
//!
//! These tests are the reason to believe the page now. Each one fails
//! against the old behaviour: the isolation test because the survivor's
//! entries are gone, the fallback test because the default cleared
//! instead of erroring.

use std::sync::Arc;

use rustango::cache::{BoxedCache, Cache, FileCache, ScopedCache};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rustango_file_cache_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Two tenants sharing one cache directory. Clearing one must leave the
/// other's entries intact — the whole promise of `ScopedCache`.
#[tokio::test]
async fn clearing_one_tenant_leaves_the_others_entries() {
    let dir = temp_dir("isolation");
    let shared: BoxedCache = Arc::new(FileCache::new(&dir));

    let acme = ScopedCache::for_tenant(shared.clone(), "acme");
    let globex = ScopedCache::for_tenant(shared.clone(), "globex");

    acme.set("stats:monthly", "acme-data", None).await.unwrap();
    globex
        .set("stats:monthly", "globex-data", None)
        .await
        .unwrap();

    // Same logical key, different tenants — both readable, unmixed.
    assert_eq!(
        acme.get("stats:monthly").await.unwrap().as_deref(),
        Some("acme-data")
    );
    assert_eq!(
        globex.get("stats:monthly").await.unwrap().as_deref(),
        Some("globex-data")
    );

    acme.clear().await.unwrap();

    assert_eq!(
        acme.get("stats:monthly").await.unwrap(),
        None,
        "the tenant that cleared must lose its own entry"
    );
    assert_eq!(
        globex.get("stats:monthly").await.unwrap().as_deref(),
        Some("globex-data"),
        "another tenant's entry must survive — this is the cross-tenant wipe"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A prefix that matches nothing must delete nothing. Guards the other
/// direction: a `delete_prefix` that over-matches, or that falls back to
/// clearing, both fail here.
#[tokio::test]
async fn a_prefix_matching_nothing_deletes_nothing() {
    let dir = temp_dir("nomatch");
    let cache = FileCache::new(&dir);

    cache.set("alpha", "1", None).await.unwrap();
    cache.set("beta", "2", None).await.unwrap();

    cache.delete_prefix("gamma").await.unwrap();

    assert_eq!(cache.get("alpha").await.unwrap().as_deref(), Some("1"));
    assert_eq!(cache.get("beta").await.unwrap().as_deref(), Some("2"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Prefix matching is on the key, not a substring of it.
#[tokio::test]
async fn prefix_matches_the_start_of_the_key_only() {
    let dir = temp_dir("startswith");
    let cache = FileCache::new(&dir);

    cache.set("tenant:acme:a", "1", None).await.unwrap();
    cache.set("x:tenant:acme:b", "2", None).await.unwrap();

    cache.delete_prefix("tenant:acme:").await.unwrap();

    assert_eq!(cache.get("tenant:acme:a").await.unwrap(), None);
    assert_eq!(
        cache.get("x:tenant:acme:b").await.unwrap().as_deref(),
        Some("2"),
        "the prefix appears mid-key here and must not match"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A file written by the pre-`RCF1` format is discarded rather than
/// served as garbage — this is how an existing cache directory migrates
/// with no migration step.
#[tokio::test]
async fn entries_in_the_old_format_are_evicted_not_misread() {
    let dir = temp_dir("oldformat");
    let cache = FileCache::new(&dir);

    // Force the directory to exist, then hand-write an old-format entry:
    // 8 bytes of expiry (0 = no TTL) followed by the raw value, no magic.
    cache.set("real", "kept", None).await.unwrap();
    let mut legacy = 0_i64.to_be_bytes().to_vec();
    legacy.extend_from_slice(b"stale-value");
    // Any filename in the right shape — the reader finds it by scan.
    std::fs::write(dir.join("deadbeef.cache"), &legacy).unwrap();

    // A scan must not trip over it, and must not resurrect it.
    cache.delete_prefix("nothing-matches-this").await.unwrap();
    assert!(
        !dir.join("deadbeef.cache").exists(),
        "an undecodable entry should be evicted by the scan, not left to rot"
    );
    assert_eq!(
        cache.get("real").await.unwrap().as_deref(),
        Some("kept"),
        "well-formed neighbours are untouched"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
