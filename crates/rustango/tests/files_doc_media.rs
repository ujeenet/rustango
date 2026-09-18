#![cfg(all(
    feature = "sqlite",
    feature = "media",
    feature = "testkit",
    feature = "admin"
))]
//! Backing test for the **media** half of `docs/files.md` — public
//! delivery, and the line between it and the management router.
//!
//! ## Why this file exists
//!
//! `files_doc.rs` is the doc-contract guard for that page, and it is
//! gated `#![cfg(all(feature = "storage", feature = "uploads"))]` — it
//! asserts the `Storage` trait and the upload guards, and **nothing at
//! all about `media`**. So roughly ninety lines of claims about the
//! manager and the router had no guard, while `docs_contract.rs`
//! reported `files.md` as covered, because coverage there is tracked
//! per *page*. A page can be half-guarded and still count.
//!
//! That is how the framing drifted far enough to need correcting: the
//! page described `media::router` as the way to serve media and never
//! mentioned any other path, so the only documented answer to "show an
//! uploaded image on a public page" was a router that refuses
//! anonymous requests by design.
//!
//! `admin` is in the gate because `media::router` is
//! `#[cfg(feature = "admin")]`. Without it this file would be a
//! compile-out rather than a skip, which is #1572's shape.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::media::router::{media_router_with, MediaAuthorizer, MediaDecision};
use rustango::media::{MediaManager, SaveOpts};
use rustango::sql::{Auto, Pool};
use rustango::storage::{InMemoryStorage, StorageRegistry};
use tower::ServiceExt as _;

/// A manager whose `default` disk has a CDN prefix, as a deployment
/// serving public media would.
async fn manager_with_cdn() -> MediaManager {
    // In-memory, and `min_connections(2)` so both connections are open
    // eagerly — a seed on one and a read on the other would fail loudly
    // if the pool did not share a single database.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite connect");
    let pool = Pool::Sqlite(pool);
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate framework media tables");
    let registry = StorageRegistry::new()
        .set("default", Arc::new(InMemoryStorage::new()))
        .with_default("default")
        .cdn("default", "https://cdn.example.com/media");
    MediaManager::new_pool(pool, registry)
}

async fn save_one(mgr: &MediaManager) -> i64 {
    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "hero/".into(),
            bytes: b"not really a png".to_vec(),
            mime: "image/png".into(),
            original_filename: "hero.png".into(),
            uploaded_by_id: None,
            collection_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("save_bytes");
    match m.id {
        Auto::Set(v) => v,
        _ => panic!("expected Auto::Set after save"),
    }
}

/// The recipe the page gives for a public page, executed.
///
/// `docs/files.md`: "Render the URL from your own handler instead:
/// `manager.public_url(media_id).await?`".
#[tokio::test]
async fn public_url_is_the_cdn_address_and_mints_no_signature() {
    let mgr = manager_with_cdn().await;
    let id = save_one(&mgr).await;

    let url = mgr
        .public_url(id)
        .await
        .expect("lookup succeeds")
        .expect("a disk with a CDN prefix yields an address");

    assert!(
        url.starts_with("https://cdn.example.com/media/"),
        "public_url did not use the configured CDN prefix: {url}"
    );
    // The page's claim is that this is stable and cacheable, which is
    // only true if nothing signed it. A presigned URL carries query
    // parameters; this must not.
    assert!(
        !url.contains('?'),
        "public_url returned something signed — the page promises a stable, \
         cacheable address, and a query string means it expires: {url}"
    );

    // Twice, to pin "stable": a signer would produce a different string
    // on the second call.
    let again = mgr.public_url(id).await.expect("lookup").expect("address");
    assert_eq!(url, again, "public_url is not stable across calls");
}

/// …and the page's two `None` cases are distinct and both real.
#[tokio::test]
async fn public_url_is_none_for_a_missing_row_and_for_a_disk_with_no_base() {
    let mgr = manager_with_cdn().await;
    assert!(
        mgr.public_url(99_999).await.expect("lookup").is_none(),
        "a missing row must be None, not an address"
    );

    // Soft-deleted counts as missing: `public_url` goes through `get`,
    // which excludes them. The page says so.
    let id = save_one(&mgr).await;
    let m = mgr.get(id).await.expect("get").expect("row");
    mgr.delete(&m).await.expect("soft delete");
    assert!(
        mgr.public_url(id).await.expect("lookup").is_none(),
        "a soft-deleted row must not keep serving a public URL"
    );

    // No CDN prefix and a backend that exposes no URL — the page's
    // other None.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite connect");
    let pool = Pool::Sqlite(pool);
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate");
    let bare = MediaManager::new_pool(
        pool,
        StorageRegistry::new()
            .set("default", Arc::new(InMemoryStorage::new()))
            .with_default("default"),
    );
    let bare_id = save_one(&bare).await;
    assert!(
        bare.public_url(bare_id).await.expect("lookup").is_none(),
        "a disk with neither a CDN prefix nor a backend URL must be None"
    );
}

/// Refuses every request, including reads — the shipped default.
struct DenyEverything;

#[rustango::media::async_trait]
impl MediaAuthorizer for DenyEverything {
    async fn authorize(
        &self,
        _parts: &axum::http::request::Parts,
        _action: rustango::media::router::MediaAction,
    ) -> MediaDecision {
        MediaDecision::Unauthenticated
    }
}

/// The other half of the page's claim: the management router is *not*
/// the public path, and stays closed for the same row.
///
/// Without this the first test would pass just as happily on a build
/// where the router served anonymous reads — which is exactly the state
/// 0.57.7 fixed, and exactly what someone reaching for an `AllowAll`
/// authorizer would recreate.
#[tokio::test]
async fn the_management_router_still_refuses_the_same_row_anonymously() {
    let mgr = manager_with_cdn().await;
    let id = save_one(&mgr).await;

    // Control: the public path serves it.
    assert!(
        mgr.public_url(id).await.expect("lookup").is_some(),
        "control failed — the row has no public URL, so the refusal below \
         would prove nothing"
    );

    let app = media_router_with(mgr, DenyEverything);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/media/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the management router served an anonymous read of a row whose public \
         URL is already available without it — mounting this router is not how \
         a public page gets media"
    );
}
