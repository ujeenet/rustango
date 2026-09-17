// `admin` is in the gate because `media::router` is `#[cfg(feature =
// "admin")]`. Without it here this file is a compile error rather than
// a skip on `--no-default-features --features sqlite,media,testkit` —
// invisible in CI, where every media job leaves default features on and
// `batteries` drags `admin` in.
#![cfg(all(
    feature = "sqlite",
    feature = "media",
    feature = "testkit",
    feature = "admin"
))]
//! `media_router` must not serve media to an unauthenticated caller.
//!
//! Security review of v0.57.6, finding 01 (High). `media_router`
//! mounted 16 routes and **not one handler took an authentication,
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
    // In-memory, not a temp file. The file version had to
    // `std::mem::forget` its `NamedTempFile` so the guard would not
    // delete the database out from under the pool — which leaked one
    // file per test, 18 per run. sqlx shares a single in-memory
    // database across the whole pool, so nothing is lost.
    // `min_connections(2)` opens both eagerly, so a seed on one and a
    // read on the other would fail loudly if the pool did not share a
    // single database. `an_authorized_read_still_works` is that control.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .connect("sqlite::memory:")
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
    // The `assert_ne!` alone, deliberately. Pinning both sides to
    // constants first and *then* asserting they differ is a tautology:
    // the comparison cannot fail, while its message carries the security
    // claim. Same shape as this file's original `assert_ne!(status, OK)`
    // defect — a assertion that reads like a guard and guards nothing.
    assert_ne!(
        collection, media,
        "a collection and a media row with the same id were indistinguishable, so \
         'may read media 1' silently authorised collection 1 — /collections/1 gave \
         {collection:?}, /media/1 gave {media:?}"
    );
}

/// The verbs are split, so a policy can allow adding without deleting.
#[tokio::test]
async fn the_verb_distinguishes_read_change_and_delete() {
    let (del, _) = action_for("/media/1", "DELETE").await;
    assert_eq!(del, vec![MediaAction::Delete(MediaTarget::Media(1))]);

    let (mv, _) = action_for("/media/1/move", "POST").await;
    assert_eq!(mv, vec![MediaAction::Change(MediaTarget::Media(1))]);

    // `NewUpload` is a `#[non_exhaustive]` struct variant, so this
    // crate can match it but cannot construct one to compare against.
    let (begin, _) = action_for("/uploads/begin", "POST").await;
    assert!(
        matches!(
            begin.as_slice(),
            [MediaAction::Add(MediaTarget::NewUpload { .. })]
        ),
        "minting an upload ticket was classified as {begin:?}"
    );
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

// =====================================================================
// Route-table coverage.
//
// The tests above assert individual paths, and a mutation survey showed
// what that misses: replacing `classify`'s body with a constant
// `Read(Media(1))` left 6 of 11 green, because a fresh database seeds
// row 1 and every id assertion used 1. Worse, reclassifying
// `DELETE /collections/{id}` as `Read` — a privilege downgrade letting
// read-only callers delete collections and orphan the media inside —
// was invisible to all 11.
//
// Ids are 7 here, never 1, so a constant cannot satisfy them.
// =====================================================================

/// Every route the router mounts, and the action the gate derives.
#[tokio::test]
async fn the_whole_route_table_reaches_the_gate_correctly() {
    // Compared through `Debug` rather than by constructing the expected
    // value: `MediaTarget::NewUpload` is a `#[non_exhaustive]` struct
    // variant, so this crate (an integration test, a separate crate)
    // can match it but cannot build one. Debug distinguishes every
    // variant and id, which is all this table needs.
    let cases: &[(&str, &str, &str)] = &[
        ("POST", "/uploads/begin", "Add(NewUpload)"),
        // Finalize mutates the media row it names — same row, so a
        // `Media` target, not a kind of its own.
        ("POST", "/uploads/7/finalize", "Change(Media(7))"),
        ("GET", "/media/7", "Read(Media(7))"),
        ("DELETE", "/media/7", "Delete(Media(7))"),
        ("POST", "/media/7/move", "Change(Media(7))"),
        ("POST", "/media/7/tags", "Change(Media(7))"),
        ("DELETE", "/media/7/tags/blue", "Change(Media(7))"),
        ("POST", "/collections", "Add(Listing)"),
        ("GET", "/collections", "Read(Listing)"),
        ("GET", "/collections/7", "Read(Collection(7))"),
        ("GET", "/collections/7/contents", "Read(Collection(7))"),
        ("DELETE", "/collections/7", "Delete(Collection(7))"),
        ("POST", "/tags", "Add(Listing)"),
        ("GET", "/tags", "Read(Listing)"),
        ("GET", "/tags/popular", "Read(Listing)"),
        ("GET", "/tags/9/media", r#"Read(Tag("9"))"#),
    ];

    let mut wrong = Vec::new();
    for (method, uri, expect) in cases {
        let (seen, _) = action_for(uri, method).await;
        let got = match seen.as_slice() {
            [one] => format!("{one:?}"),
            other => format!("{other:?}"),
        };
        if got != *expect {
            wrong.push(format!(
                "{method} {uri}: the handler serves this route, the gate was told \
                 {got} — expected {expect}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the gate and the route table disagree:\n  {}",
        wrong.join("\n  ")
    );
}

/// `+` is literal in a path segment, never a space.
///
/// Swapping `percent_decode_path` for the form-semantics `url_decode`
/// used to change nothing in this file. It would authorize tag `a b`
/// while the handler serves `a+b` — the same guard-the-wrong-row shape
/// as `%31`, in the one target that is caller-chosen text.
#[tokio::test]
async fn a_plus_in_a_slug_is_not_a_space() {
    let (seen, _) = action_for("/tags/a+b/media", "GET").await;
    assert_eq!(
        seen,
        vec![MediaAction::Read(MediaTarget::Tag("a+b".into()))],
        "the gate form-decoded the slug, so it is deciding about a different tag \
         than the one the handler looks up"
    );
}

/// What axum's own `Path` hands the handler, so the test above rests on
/// a measurement rather than an assumption about axum.
#[tokio::test]
async fn axum_path_decoding_is_what_the_gate_must_match() {
    async fn echo(axum::extract::Path(slug): axum::extract::Path<String>) -> String {
        slug
    }
    let app = axum::Router::new().route("/tags/{slug}/media", axum::routing::get(echo));
    for (uri, expect) in [("/tags/a+b/media", "a+b"), ("/tags/a%2Bb/media", "a+b")] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            expect,
            "axum decoding of {uri}"
        );
    }
}

/// A recursive listing reaches rows the named collection does not hold.
///
/// Same verb, same target, different query string, wider reach: it
/// returns media living in descendant collections the authorizer was
/// never asked about. It classifies as `Listing`, whose docs say
/// granting it is not harmless.
#[tokio::test]
async fn a_recursive_listing_is_not_a_single_collection_read() {
    let (plain, _) = action_for("/collections/7/contents", "GET").await;
    assert_eq!(
        plain,
        vec![MediaAction::Read(MediaTarget::Collection(7))],
        "a non-recursive contents read names one collection"
    );

    for uri in [
        "/collections/7/contents?recursive=true",
        "/collections/7/contents?recursive=1",
        "/collections/7/contents?limit=5&recursive=true",
    ] {
        let (seen, _) = action_for(uri, "GET").await;
        assert_eq!(
            seen,
            vec![MediaAction::Read(MediaTarget::Listing)],
            "{uri} widens past the collection it names, but the gate was told \
             {seen:?} — a policy granting that one collection would authorise \
             descendants it never saw"
        );
    }
}

/// HEAD must reach the gate as the GET it is routed to.
///
/// axum maps HEAD onto the GET handler. A gate matching `Method::GET`
/// only refuses HEAD on a route the caller is allowed to read — fail
/// closed, but it makes a documented route unusable.
#[tokio::test]
async fn head_is_classified_as_the_read_it_becomes() {
    let (seen, status) = action_for("/media/1", "HEAD").await;
    assert_eq!(
        seen,
        vec![MediaAction::Read(MediaTarget::Media(1))],
        "HEAD did not reach the gate as a read"
    );
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "HEAD was refused for a caller the policy allows"
    );
}

/// The mount the quick start documents.
///
/// Every other test calls `oneshot` on the bare router, but the docs say
/// `.nest("/media", ...)`, and `classify` depends entirely on `nest`
/// stripping the prefix before the layer sees the path. Failure here
/// would be fail-closed — 403 on everything — but total, and nothing
/// else exercises it.
#[tokio::test]
async fn the_documented_nested_mount_works() {
    let mgr = manager().await;
    let id = seed(&mgr).await;
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app =
        axum::Router::new().nest("/media", media_router_with(mgr, Recorder(Arc::clone(&log))));

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/media/media/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    assert_eq!(
        log.lock().unwrap().clone(),
        vec![MediaAction::Read(MediaTarget::Media(id))],
        "nested, the gate saw something other than the stripped path — every route \
         would refuse"
    );
    assert_eq!(resp.status(), StatusCode::OK);
}

/// `Arc<dyn MediaAuthorizer>` must satisfy the constructor, so a host
/// can choose its policy at runtime.
#[tokio::test]
async fn a_boxed_authorizer_can_be_mounted() {
    let mgr = manager().await;
    let id = seed(&mgr).await;
    let policy: Arc<dyn MediaAuthorizer> = Arc::new(AllowAll);
    let app = media_router_with(mgr, policy);

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/media/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");
    assert_eq!(resp.status(), StatusCode::OK);
}

// =====================================================================
// Presigned URLs are bearer credentials, so nothing may cache them.
//
// RFC 9111 §3.5 keeps a shared cache off a response whose request
// carried `Authorization`, and says nothing about `Cookie` — while the
// authorizer this module documents is cookie/session shaped. A 200 GET
// with no explicit freshness is heuristically cacheable (§4.2.2), and
// with no `Vary` the cache key is method plus URI. So a CDN in front of
// a cookie-authenticated deployment could serve one user's signed link
// to the next caller of the same URI.
// =====================================================================

fn cache_control(resp: &axum::http::Response<Body>) -> String {
    resp.headers()
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<absent>")
        .to_owned()
}

#[tokio::test]
async fn a_served_media_row_is_never_cached() {
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

    assert_eq!(resp.status(), StatusCode::OK, "control: the row must serve");
    assert_eq!(
        cache_control(&resp),
        "no-store",
        "this body can carry a presigned URL — a bearer credential valid to anyone \
         holding it. Without a directive a shared cache may store it and replay it \
         to a different user."
    );
}

/// Every route, not just the ones that embed a signature today.
#[tokio::test]
async fn the_whole_surface_is_uncacheable() {
    let cases = [
        ("GET", "/collections"),
        ("GET", "/tags"),
        ("GET", "/tags/popular"),
        ("GET", "/collections/1/contents"),
        ("GET", "/tags/blue/media"),
    ];
    let mut missing = Vec::new();
    for (method, uri) in cases {
        let mgr = manager().await;
        let _ = seed(&mgr).await;
        let app = media_router_with(mgr, AllowAll);
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
        let cc = cache_control(&resp);
        if cc != "no-store" {
            missing.push(format!("{method} {uri} -> {cc}"));
        }
    }
    assert!(
        missing.is_empty(),
        "these responses are cacheable, and a route added later must not be able to \
         opt out silently:\n  {}",
        missing.join("\n  ")
    );
}

/// The refusal path too — a cached 403 is its own bug.
#[tokio::test]
async fn a_refusal_is_not_cached_either() {
    let mgr = manager().await;
    let id = seed(&mgr).await;
    #[allow(deprecated)]
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

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        cache_control(&resp),
        "no-store",
        "a cached 403 would outlive the policy decision that produced it"
    );
}

// =====================================================================
// The contents listing is bounded, and costs one tag query per page.
//
// `list_in_collection` emitted no `LIMIT`, and the handler called
// `tags_for` once per row. A 500-row collection returned 214 563 bytes
// from a ~215-byte request — ~1000x amplification, with the row count
// set by how much media the deployment holds rather than by anything
// the server controls. `?recursive=true` widened it across the subtree.
// =====================================================================

async fn seed_n(mgr: &MediaManager, collection_id: Option<i64>, n: usize) -> Vec<i64> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let m = mgr
            .save_bytes(SaveOpts {
                disk: "default".into(),
                key_prefix: "bulk/".into(),
                bytes: format!("row {i}").into_bytes(),
                mime: "text/plain".into(),
                original_filename: format!("f{i}.txt"),
                uploaded_by_id: Some(1),
                collection_id,
                metadata: serde_json::json!({}),
            })
            .await
            .expect("seed");
        if let rustango::sql::Auto::Set(v) = m.id {
            ids.push(v);
        }
    }
    ids
}

async fn contents_len(app: axum::Router, uri: &str) -> usize {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router answers");
    assert_eq!(resp.status(), StatusCode::OK, "{uri} did not serve");
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .expect("body");
    serde_json::from_slice::<Vec<serde_json::Value>>(&bytes)
        .expect("json array")
        .len()
}

/// Without a `limit`, the listing still has a ceiling.
#[tokio::test]
async fn the_contents_listing_is_bounded_by_default() {
    let mgr = manager().await;
    let c = mgr
        .create_collection("Bulk", "bulk", None, "")
        .await
        .expect("collection");
    let cid = match c.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    seed_n(&mgr, Some(cid), 150).await;
    let app = media_router_with(mgr, AllowAll);

    let n = contents_len(app, &format!("/collections/{cid}/contents")).await;
    assert!(
        n <= 100,
        "an unpaged contents listing returned {n} rows — the row count is set by how \
         much media the deployment holds, not by the server"
    );
}

/// A caller-supplied `limit` is honoured, and clamped.
#[tokio::test]
async fn a_contents_limit_is_honoured_and_clamped() {
    let mgr = manager().await;
    let c = mgr
        .create_collection("Bulk", "bulk", None, "")
        .await
        .expect("collection");
    let cid = match c.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    seed_n(&mgr, Some(cid), 40).await;
    let app = media_router_with(mgr, AllowAll);

    assert_eq!(
        contents_len(
            app.clone(),
            &format!("/collections/{cid}/contents?limit=10")
        )
        .await,
        10,
        "an explicit limit was not honoured"
    );
    // `LIMIT -1` means *no limit* on SQLite, so a negative value must
    // never reach the database as-is.
    let neg = contents_len(
        app.clone(),
        &format!("/collections/{cid}/contents?limit=-1"),
    )
    .await;
    assert_eq!(
        neg, 1,
        "a negative limit was not clamped — on SQLite that is an unbounded query \
         wearing a limit, and it returned {neg} rows"
    );
    let huge = contents_len(
        app,
        &format!("/collections/{cid}/contents?limit=9223372036854775807"),
    )
    .await;
    assert!(huge <= 40, "an enormous limit returned {huge} rows");
}

/// Offset pages rather than repeating page one.
#[tokio::test]
async fn a_contents_offset_pages() {
    let mgr = manager().await;
    let c = mgr
        .create_collection("Bulk", "bulk", None, "")
        .await
        .expect("collection");
    let cid = match c.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    seed_n(&mgr, Some(cid), 12).await;
    let app = media_router_with(mgr, AllowAll);

    assert_eq!(
        contents_len(app.clone(), &format!("/collections/{cid}/contents?limit=5")).await,
        5
    );
    assert_eq!(
        contents_len(
            app,
            &format!("/collections/{cid}/contents?limit=5&offset=10")
        )
        .await,
        2,
        "offset did not move the window"
    );
}

/// One tag query for the page, not one per row.
#[tokio::test]
async fn tags_for_many_batches_the_whole_page() {
    let mgr = manager().await;
    let ids = seed_n(&mgr, None, 5).await;
    for (i, id) in ids.iter().enumerate() {
        mgr.tag(*id, &[format!("t{i}").as_str()])
            .await
            .expect("tag");
    }

    let batched = mgr.tags_for_many(&ids).await.expect("batched");
    assert_eq!(batched.len(), 5, "a media id with tags went missing");
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            batched.get(id).map(Vec::as_slice),
            Some([format!("t{i}")].as_slice()),
            "batched tags disagree with what was written for {id}"
        );
    }

    // Agrees with the per-row call it replaces.
    for id in &ids {
        let one: Vec<String> = mgr
            .tags_for(*id)
            .await
            .expect("per-row")
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert_eq!(batched.get(id), Some(&one), "batched != per-row for {id}");
    }

    // An id with no tags is absent, not an empty entry.
    let untagged = seed_n(&mgr, None, 1).await;
    let m = mgr.tags_for_many(&untagged).await.expect("untagged");
    assert!(m.is_empty(), "an untagged row produced an entry: {m:?}");

    assert!(
        mgr.tags_for_many(&[]).await.expect("empty").is_empty(),
        "an empty id list must not query at all"
    );
}
