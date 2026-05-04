//! Cookbook Chapter 11 — async / IO / extensions.
//!
//! Pure-API tests against the framework's extension surface — caches,
//! webhook signatures, signed URLs, scheduler. No DB, no network.
//!
//! Run: `cargo test --test cookbook_chapter11_extensions`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// §11.135 — InMemoryCache get_or_set lazily computes + memoizes.
#[tokio::test]
async fn cache_get_or_set_memoizes_loader() {
    use rustango::cache::{Cache, InMemoryCache};
    let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
    let calls = Arc::new(AtomicUsize::new(0));

    for _ in 0..3 {
        let c = calls.clone();
        let v: i64 = rustango::cache::get_or_set(
            &*cache,
            "answer",
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                42i64
            },
            Some(std::time::Duration::from_secs(60)),
        )
        .await
        .expect("get_or_set");
        assert_eq!(v, 42);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "loader runs exactly once");
}

// §11.135 — Cache::set / get round-trip JSON values.
#[tokio::test]
async fn cache_set_get_json_round_trips() {
    use rustango::cache::{set_json, get_json, InMemoryCache, Cache};
    let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
    set_json(&*cache, "user:42", &serde_json::json!({"name": "ada", "tier": "pro"}), None)
        .await.expect("set_json");
    let back: serde_json::Value = get_json(&*cache, "user:42")
        .await.expect("get_json")
        .expect("Some(value)");
    assert_eq!(back["name"], "ada");
    assert_eq!(back["tier"], "pro");
}

// §11.128 — webhook signing + signature verification.
#[test]
fn webhook_sign_then_verify_round_trip() {
    use rustango::webhook::{sign, verify_signature, SignatureFormat};
    let secret = b"webhook-secret-32-bytes-or-more!!!!!!!!";
    let body = b"{\"event\":\"post.created\",\"id\":42}";
    let sig = sign(SignatureFormat::HexSha256, secret, body);
    assert!(verify_signature(SignatureFormat::HexSha256, secret, body, &sig));
    assert!(!verify_signature(SignatureFormat::HexSha256, secret, b"tampered", &sig));
}

// §11.128 — GitHub-style `sha256=<hex>` prefix format.
#[test]
fn webhook_github_prefix_format() {
    use rustango::webhook::{sign, verify_signature, SignatureFormat};
    let secret = b"webhook-secret-32-bytes-or-more!!!!!!!!";
    let body = b"{\"event\":\"x\"}";
    let sig = sign(SignatureFormat::HexSha256WithPrefix, secret, body);
    assert!(sig.starts_with("sha256="), "github prefix shape, got {sig}");
    assert!(verify_signature(SignatureFormat::HexSha256WithPrefix, secret, body, &sig));
}

// §11.92 — signed URL: sign + verify_at honors expiry.
#[test]
fn signed_url_sign_then_verify_at_respects_expiry() {
    use rustango::signed_url::{sign_at, verify_at};
    let secret = b"signed-url-32-bytes-or-more!!!!!!!!!!!!!";
    let signed = sign_at(
        "https://blog.example.com/admin/import?file=2026-Q2.csv",
        secret,
        Some(2_000),
    );
    verify_at(&signed, secret, 1_500).expect("not yet expired at 1500");
    verify_at(&signed, secret, 2_500).expect_err("expired at 2500");

    let other = b"different-secret-32-bytes-or-more!!!!!!!";
    verify_at(&signed, other, 1_500).expect_err("wrong secret rejected");
}

// §11.92 — signed URL without expiry (`None` ttl) verifies forever.
#[test]
fn signed_url_no_expiry_always_verifies() {
    use rustango::signed_url::{sign, verify};
    let secret = b"signed-url-32-bytes-or-more!!!!!!!!!!!!!";
    let signed = sign("https://blog.example.com/feed.atom", secret, None);
    verify(&signed, secret).expect("no-expiry signed URL verifies");
}

// §11.126 — Scheduler::every fires its job at the given period.
#[tokio::test]
async fn scheduler_every_fires_periodic_job() {
    use rustango::scheduler::Scheduler;
    use std::time::Duration;

    let s = Scheduler::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    s.every("tick", Duration::from_millis(15), move || {
        let c = c.clone();
        async move { c.fetch_add(1, Ordering::SeqCst); }
    });
    let handle = s.start();

    tokio::time::sleep(Duration::from_millis(80)).await;
    let snap_before = counter.load(Ordering::SeqCst);
    assert!(snap_before >= 3, "should have fired ≥3 times in 80ms; got {snap_before}");

    handle.shutdown().await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let snap_after = counter.load(Ordering::SeqCst);
    assert!(
        snap_after - snap_before <= 1,
        "after shutdown counter should be stable; before={snap_before} after={snap_after}",
    );
}
