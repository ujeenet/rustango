#![cfg(all(feature = "sqlite", feature = "media", feature = "testkit"))]
//! `media_router` must not serve media to an unauthenticated caller.
//!
//! Security review of v0.57.6, finding 01 (High). `media_router`
//! mounted 15 routes and **not one handler took an authentication,
//! authorization, or tenant extractor** — every signature was
//! `State(manager)` plus a path or body. `MediaManager` holds a single
//! `sql::Pool`, so the surface was not tenant-scoped either.
//!
//! What that gave an unauthenticated caller:
//!
//! - `GET /media/{id}` returned the row **and a presigned S3 GET URL**.
//!   Walk the integer id space and harvest signed download links for
//!   every object in the bucket.
//! - `DELETE /media/{id}` deleted by id with no ownership check.
//! - `POST /uploads/begin` minted a presigned **PUT** for a
//!   caller-chosen disk and key prefix — an unauthenticated write
//!   primitive into the app's storage.
//!
//! A `.layer(auth)` in front could not fix the multi-tenant half:
//! with no tenant on any handler, an authenticated tenant-A user still
//! reads tenant B's row by id.
//!
//! These tests assert the *closed* behaviour. Run them against the
//! pre-fix router and the first one returns 200 with a body.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::media::{MediaManager, SaveOpts};
use rustango::sql::Pool;
use rustango::storage::{InMemoryStorage, StorageRegistry};
use tower::ServiceExt as _;

use rustango::media::router::{media_router_with, MediaAction, MediaAuthorizer, MediaTarget};

async fn manager() -> MediaManager {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().display());
    std::mem::forget(tmp);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("sqlite connect");
    let pool_enum = Pool::Sqlite(pool);
    rustango::testkit::migrate_framework(&pool_enum)
        .await
        .expect("migrate framework media tables");
    let registry = StorageRegistry::new()
        .set("default", Arc::new(InMemoryStorage::new()))
        .with_default("default");
    MediaManager::new_pool(pool_enum, registry)
}

/// Put one row in so a successful read would have something to return.
/// Without this an empty table could make a 404 look like a refusal.
async fn seed(mgr: &MediaManager) -> i64 {
    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "secret/".into(),
            bytes: b"not yours".to_vec(),
            mime: "text/plain".into(),
            original_filename: "secret.txt".into(),
            uploaded_by_id: Some(1),
            collection_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("seed media row");
    match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!("seeded row has no id"),
    }
}

/// An anonymous `GET /media/{id}` must not return the row.
// Calls the deprecated constructor on purpose — proving it refuses is
// the point of the test.
#[allow(deprecated)]
#[tokio::test]
async fn an_anonymous_read_is_refused() {
    let mgr = manager().await;
    let id = seed(&mgr).await;
    let app = rustango::media::router::media_router(mgr);

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/media/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "an unauthenticated GET /media/{id} returned 200 — the row, and with it a \
         presigned download URL, is readable by anyone who can guess an integer"
    );
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401/403, got {} — the refusal should say why",
        resp.status()
    );
}

/// An anonymous `DELETE /media/{id}` must not destroy the row.
#[allow(deprecated)]
#[tokio::test]
async fn an_anonymous_delete_is_refused() {
    let mgr = manager().await;
    let id = seed(&mgr).await;
    let app = rustango::media::router::media_router(mgr);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/media/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    // 401/403 specifically, not merely "not 200".
    //
    // The first version of this test asserted `!= OK` and **passed while
    // the vulnerability fired**: the handler returns `204 No Content` on
    // a successful delete, and 204 is not 200. A test that green-lights
    // the exact behaviour it exists to forbid is worse than no test.
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "an unauthenticated DELETE /media/{id} was not refused — got {}. 204 here \
         means the row was destroyed: by id, with no ownership check.",
        resp.status()
    );
}

/// An anonymous `POST /uploads/begin` must not mint a presigned PUT.
///
/// This is the write primitive: a caller-chosen `disk` and
/// `key_prefix`, with caller-supplied attribution.
#[allow(deprecated)]
#[tokio::test]
async fn an_anonymous_upload_ticket_is_refused() {
    let mgr = manager().await;
    let app = rustango::media::router::media_router(mgr);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/uploads/begin")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"disk":"default","key_prefix":"../etc/","mime":"text/plain",
                        "original_filename":"x.txt","size_bytes":1,"uploaded_by_id":9999}"#,
                ))
                .unwrap(),
        )
        .await
        .expect("router answers");

    // Again 401/403 specifically. `!= OK` passed here too, on a 400 —
    // the request failed somewhere in body handling, which is not a
    // refusal and would have kept passing if authorization never ran.
    // Authorization must be decided before the body is processed.
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "an unauthenticated POST /uploads/begin was not refused — got {}. This route \
         mints a presigned PUT for a caller-chosen disk and key prefix, so an \
         unrefused call is a write primitive into the app's storage.",
        resp.status()
    );
}

/// The control: a permissive authorizer still serves.
///
/// Without this, every test above is satisfied by a router that refuses
/// unconditionally — which would be "secure" and useless. This proves
/// the gate is a gate and not a wall.
struct AllowAll;

#[async_trait::async_trait]
impl MediaAuthorizer for AllowAll {
    async fn authorize(&self, _: &axum::http::request::Parts, _: MediaAction) -> bool {
        true
    }
}

#[tokio::test]
async fn an_authorized_read_still_works() {
    let mgr = manager().await;
    let id = seed(&mgr).await;
    let app = media_router_with(mgr, AllowAll);

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
        StatusCode::OK,
        "a permissive authorizer must still serve the row — otherwise the fix is a \
         wall, and the refusal tests above prove nothing about authorization"
    );
}

/// Records every action the authorizer is handed, and allows, so the
/// handler still runs and the two can be compared.
struct Recorder(Arc<std::sync::Mutex<Vec<MediaAction>>>);

#[async_trait::async_trait]
impl MediaAuthorizer for Recorder {
    async fn authorize(&self, _: &axum::http::request::Parts, action: MediaAction) -> bool {
        self.0.lock().unwrap().push(action);
        true
    }
}

/// Send `uri` through a permissive router and report what the
/// authorizer was told, plus what the handler answered.
async fn action_for(uri: &str, method: &str) -> (Vec<MediaAction>, StatusCode) {
    let mgr = manager().await;
    let _ = seed(&mgr).await;
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = media_router_with(mgr, Recorder(Arc::clone(&log)));
    let resp = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");
    let status = resp.status();
    let seen = log.lock().unwrap().clone();
    (seen, status)
}

/// The authorizer sees the object id, so it can decide per row.
///
/// This is the half a blanket `.layer(auth)` in front of the router
/// cannot do: with no id, an authenticated tenant-A user still reads
/// tenant B's row.
#[tokio::test]
async fn the_authorizer_receives_the_object_id() {
    let mgr = manager().await;
    let id = seed(&mgr).await;
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = media_router_with(mgr, Recorder(Arc::clone(&log)));

    let _ = app
        .oneshot(
            Request::builder()
                .uri(format!("/media/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![MediaAction::Read(MediaTarget::Media(id))],
        "the authorizer was not told which row the request names, so it cannot make \
         an object-level or tenant-scoped decision"
    );
}

// =====================================================================
// The gate must decide about the same row the handler serves.
//
// Each of these three was a live bypass in the first version of this
// fix, reproduced by running it. `classify` scanned the **raw** path
// for the first integer-parsable segment, so the authorizer and the
// handler disagreed about which row — and a gate that guards a
// different row than the one served is not a gate.
// =====================================================================

/// `%31` is `1`. axum's `Path` extractor decodes it; the gate must too.
///
/// Measured against the first version: the authorizer was handed
/// `Read { id: None }` while the handler returned **200 and row 1**,
/// including its presigned download URL. The documented example
/// mapped `id: None` to `true`, so copying the docs re-opened the
/// exact hole this file exists to close.
#[tokio::test]
async fn a_percent_encoded_id_is_still_the_id() {
    let (seen, _) = action_for("/media/%31", "GET").await;
    assert_eq!(
        seen,
        vec![MediaAction::Read(MediaTarget::Media(1))],
        "a percent-encoded id did not reach the authorizer as an id. The handler \
         decodes it and serves the row, so the gate is deciding about something \
         the request is not."
    );
}

/// A tag slug is caller-chosen text, never an object id.
#[tokio::test]
async fn a_numeric_tag_slug_is_not_an_object_id() {
    let (seen, _) = action_for("/tags/2024/media", "GET").await;
    assert_eq!(
        seen,
        vec![MediaAction::Read(MediaTarget::Tag("2024".into()))],
        "a numeric tag slug was handed over as an object id — an attacker picks the \
         slug, so that forges any id the authorizer will trust"
    );
}

/// `/collections/1` and `/media/1` are different rows in different
/// tables. One untyped integer cannot tell a policy which.
#[tokio::test]
async fn a_collection_id_is_not_a_media_id() {
    let (collection, _) = action_for("/collections/1", "GET").await;
    let (media, _) = action_for("/media/1", "GET").await;
    assert_eq!(
        collection,
        vec![MediaAction::Read(MediaTarget::Collection(1))]
    );
    assert_eq!(media, vec![MediaAction::Read(MediaTarget::Media(1))]);
    assert_ne!(
        collection, media,
        "a collection and a media row with the same id were indistinguishable, so \
         'may read media 1' silently authorised collection 1"
    );
}

/// The verbs are split, so a policy can allow adding without deleting.
#[tokio::test]
async fn the_verb_distinguishes_read_change_and_delete() {
    let (del, _) = action_for("/media/1", "DELETE").await;
    assert_eq!(del, vec![MediaAction::Delete(MediaTarget::Media(1))]);

    let (mv, _) = action_for("/media/1/move", "POST").await;
    assert_eq!(mv, vec![MediaAction::Change(MediaTarget::Media(1))]);

    let (begin, _) = action_for("/uploads/begin", "POST").await;
    assert_eq!(begin, vec![MediaAction::Add(MediaTarget::NewUpload)]);
}

/// `popular` is a listing, not a tag named "popular".
#[tokio::test]
async fn the_popular_listing_is_not_a_tag_lookup() {
    let (seen, _) = action_for("/tags/popular", "GET").await;
    assert_eq!(seen, vec![MediaAction::Read(MediaTarget::Listing)]);
}

/// A shape that matches no route is refused, not passed through.
#[tokio::test]
async fn an_unrecognised_shape_is_refused() {
    // Permissive authorizer: if this 403s, the refusal came from
    // classification failing closed rather than from the policy.
    let (seen, status) = action_for("/media/not-a-number", "GET").await;
    assert!(
        seen.is_empty(),
        "an unparseable id reached the authorizer as a target: {seen:?}"
    );
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an unrecognised path shape was not refused — falling through to a default \
         is how a gate gets walked around"
    );
}
