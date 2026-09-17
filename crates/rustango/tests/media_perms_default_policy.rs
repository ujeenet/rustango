// `Pool` is single-variant in a sqlite-only build, so the `let ... else`
// in `make_user` is irrefutable here and refutable on multi-backend
// builds. Same allow, same reason, as `permissions_sqlite_live.rs`.
#![allow(irrefutable_let_patterns)]
// `admin` because `media::router` is `#[cfg(feature = "admin")]`, and
// `tenancy` because `MediaPerms` is. Spelled out rather than left to
// default features so this is a skip, not a silent compile-out.
#![cfg(all(
    feature = "sqlite",
    feature = "media",
    feature = "testkit",
    feature = "admin",
    feature = "tenancy"
))]
//! `MediaPerms` — the default policy, so the secure path is the
//! one-liner (#1546).
//!
//! `media_router` refuses every request, and the fastest way back to
//! green from those 403s is an `AllowAll` trait impl: the original hole
//! with extra steps. These tests are what make the shipped alternative
//! worth reaching for — and what stops it quietly granting more than it
//! reads like.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::media::router::{media_router_with, MediaPerms};
use rustango::media::{MediaManager, SaveOpts};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::storage::{InMemoryStorage, StorageRegistry};
use rustango::tenancy::middleware::AuthenticatedUser;
use rustango::tenancy::permissions::set_user_perm_pool;
use tower::ServiceExt as _;

/// A manager and the pool `MediaPerms` reads grants from — the same
/// pool, which is the single-tenant shape the type documents.
async fn setup() -> (MediaManager, Pool) {
    let sq = sqlx::sqlite::SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite connect");
    let pool = Pool::Sqlite(sq);
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate framework");
    let registry = StorageRegistry::new()
        .set("default", Arc::new(InMemoryStorage::new()))
        .with_default("default");
    (MediaManager::new_pool(pool.clone(), registry), pool)
}

async fn make_user(pool: &Pool, name: &str, superuser: bool) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!("sqlite-only suite")
    };
    sqlx::query(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES (?, '', ?, 1, datetime('now'))",
    )
    .bind(name)
    .bind(i64::from(superuser))
    .execute(sq)
    .await
    .expect("insert user");
    sqlx::query_scalar::<_, i64>("SELECT id FROM rustango_users WHERE username = ?")
        .bind(name)
        .fetch_one(sq)
        .await
        .expect("read back user id")
}

async fn grant(pool: &Pool, uid: i64, codename: &str) {
    set_user_perm_pool(uid, codename, true, pool)
        .await
        .expect("grant");
}

async fn seed_media(mgr: &MediaManager) -> i64 {
    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "t/".into(),
            bytes: b"x".to_vec(),
            mime: "text/plain".into(),
            original_filename: "x.txt".into(),
            uploaded_by_id: Some(1),
            collection_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("seed media");
    match m.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    }
}

/// The router under `MediaPerms`, with `user` injected the way
/// `require_auth` injects it. `None` is an anonymous request.
fn app(mgr: MediaManager, pool: Pool, user: Option<AuthenticatedUser>) -> axum::Router {
    let router = media_router_with(mgr, MediaPerms::new(pool));
    router.layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| {
            let user = user.clone();
            async move {
                if let Some(u) = user {
                    req.extensions_mut().insert(u);
                }
                next.run(req).await
            }
        },
    ))
}

fn principal(id: i64, superuser: bool) -> AuthenticatedUser {
    AuthenticatedUser {
        id,
        username: format!("u{id}"),
        is_superuser: superuser,
    }
}

async fn get_media(
    mgr: MediaManager,
    pool: Pool,
    user: Option<AuthenticatedUser>,
    id: i64,
) -> StatusCode {
    app(mgr, pool, user)
        .oneshot(
            Request::builder()
                .uri(format!("/media/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers")
        .status()
}

// =====================================================================
// 401 / 403 / 200
// =====================================================================

/// No principal is 401, not 403, so a token client refreshes rather
/// than treating the refusal as final. It is also what a router mounted
/// *outside* `require_auth` looks like — the symptom names its cause.
#[tokio::test]
async fn an_anonymous_request_is_401() {
    let (mgr, pool) = setup().await;
    let id = seed_media(&mgr).await;
    assert_eq!(
        get_media(mgr, pool, None, id).await,
        StatusCode::UNAUTHORIZED,
        "an anonymous request got something other than 401. A token client treats \
         401 as its cue to refresh; anything else and the member is silently \
         logged out"
    );
}

/// A signed-in user without the codename is 403 — the other half, and
/// the one that proves the 401 above is not just "the gate refuses".
#[tokio::test]
async fn a_principal_without_the_codename_is_403() {
    let (mgr, pool) = setup().await;
    let id = seed_media(&mgr).await;
    let uid = make_user(&pool, "nobody", false).await;
    assert_eq!(
        get_media(mgr, pool, Some(principal(uid, false)), id).await,
        StatusCode::FORBIDDEN,
        "a signed-in user without `rustango_media.view` was not refused with 403"
    );
}

/// …and the grant actually serves the row. Without this the policy
/// could be a wall and every refusal above would prove nothing.
#[tokio::test]
async fn the_view_codename_serves_the_row() {
    let (mgr, pool) = setup().await;
    let id = seed_media(&mgr).await;
    let uid = make_user(&pool, "reader", false).await;
    grant(&pool, uid, "rustango_media.view").await;
    assert_eq!(
        get_media(mgr, pool, Some(principal(uid, false)), id).await,
        StatusCode::OK,
        "a user holding `rustango_media.view` was refused — the default policy is a \
         wall, not a gate"
    );
}

/// Superusers short-circuit, matching every other gate here.
#[tokio::test]
async fn a_superuser_needs_no_grant() {
    let (mgr, pool) = setup().await;
    let id = seed_media(&mgr).await;
    let uid = make_user(&pool, "root", true).await;
    assert_eq!(
        get_media(mgr, pool, Some(principal(uid, true)), id).await,
        StatusCode::OK,
        "a superuser was refused"
    );
}

// =====================================================================
// One codename per create target
// =====================================================================

/// `POST /collections` and `POST /tags` used to arrive as the same
/// `Add(Listing)`. Under codenames that would mean one grant covering
/// both tables — and collections nest, so "may label things" would buy
/// a foothold under someone else's tree.
#[tokio::test]
async fn creating_a_collection_does_not_grant_creating_a_tag() {
    let (mgr, pool) = setup().await;
    let uid = make_user(&pool, "folders", false).await;
    grant(&pool, uid, "rustango_media_collections.add").await;
    let app = app(mgr, pool, Some(principal(uid, false)));

    let made = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/collections")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"X","slug":"x"}"#))
                .unwrap(),
        )
        .await
        .expect("router answers");
    assert_ne!(
        made.status(),
        StatusCode::FORBIDDEN,
        "control: `rustango_media_collections.add` must create a collection, or the \
         assertion below is measuring a policy that refuses everything"
    );

    let tag = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tags")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"slug":"blue"}"#))
                .unwrap(),
        )
        .await
        .expect("router answers");
    assert_eq!(
        tag.status(),
        StatusCode::FORBIDDEN,
        "the collection-create grant also created a tag — one codename covering two \
         tables is the confusion `Media(7)` vs `Collection(7)` was split to remove"
    );
}

// =====================================================================
// The subtree delete writes to two tables, so it needs two codenames
// =====================================================================

async fn collection_with_media(mgr: &MediaManager) -> (i64, i64) {
    let c = mgr
        .create_collection("Parent", "p", None, "")
        .await
        .expect("collection");
    let cid = match c.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "t/".into(),
            bytes: b"x".to_vec(),
            mime: "text/plain".into(),
            original_filename: "x.txt".into(),
            uploaded_by_id: Some(1),
            collection_id: Some(cid),
            metadata: serde_json::json!({}),
        })
        .await
        .expect("seed media");
    let mid = match m.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    (cid, mid)
}

/// Deleting a collection soft-deletes every descendant **and** sets
/// `collection_id = NULL` on the media inside them. That is a write to
/// `rustango_media`, so a caller who may not change media rows must not
/// be able to do it by deleting the folder they sit in.
///
/// #1558 made this a distinct target; this is the same point in
/// codenames.
#[tokio::test]
async fn deleting_a_subtree_also_needs_the_media_change_codename() {
    let (mgr, pool) = setup().await;
    let (cid, _mid) = collection_with_media(&mgr).await;
    let uid = make_user(&pool, "folder-deleter", false).await;
    grant(&pool, uid, "rustango_media_collections.delete").await;

    let resp = app(mgr, pool, Some(principal(uid, false)))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/collections/{cid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "`rustango_media_collections.delete` alone deleted a subtree. That route \
         re-parents every media row underneath, so it writes to `rustango_media` — \
         a caller without `rustango_media.change` reached rows it may not touch"
    );
}

/// Both grants together do the delete. Without this the check above is
/// satisfied by a policy that refuses the route outright.
#[tokio::test]
async fn both_subtree_codenames_together_delete() {
    let (mgr, pool) = setup().await;
    let (cid, _mid) = collection_with_media(&mgr).await;
    let uid = make_user(&pool, "librarian", false).await;
    grant(&pool, uid, "rustango_media_collections.delete").await;
    grant(&pool, uid, "rustango_media.change").await;

    let resp = app(mgr, pool, Some(principal(uid, false)))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/collections/{cid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "holding both required codenames was still refused — the two-codename rule \
         is a wall, not a gate"
    );
}

/// `rustango_media.change` on its own is not a collection delete
/// either. Requiring *all* of a list is only meaningful if neither
/// member alone suffices.
#[tokio::test]
async fn the_media_change_codename_alone_is_not_a_subtree_delete() {
    let (mgr, pool) = setup().await;
    let (cid, _mid) = collection_with_media(&mgr).await;
    let uid = make_user(&pool, "editor", false).await;
    grant(&pool, uid, "rustango_media.change").await;

    let resp = app(mgr, pool, Some(principal(uid, false)))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/collections/{cid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answers");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "`rustango_media.change` alone deleted a collection subtree, so the codename \
         list is being read as ANY rather than ALL"
    );
}

// =====================================================================
// Fail closed
// =====================================================================

/// A driver error is not a grant. Reading the permission tables can
/// fail — a migration mid-flight, a dropped table, a connection
/// blip — and treating that as "allow" turns it into an open bucket.
#[tokio::test]
async fn a_failed_permission_lookup_refuses() {
    let (mgr, pool) = setup().await;
    let id = seed_media(&mgr).await;
    let uid = make_user(&pool, "reader", false).await;
    grant(&pool, uid, "rustango_media.view").await;

    // Control first: the grant works while the catalog is intact.
    assert_eq!(
        get_media(mgr.clone(), pool.clone(), Some(principal(uid, false)), id).await,
        StatusCode::OK,
        "control: the grant must work before the table is dropped, or this test \
         passes for the wrong reason"
    );

    rustango::sql::raw_execute_pool(&pool, "DROP TABLE rustango_user_permissions", Vec::new())
        .await
        .expect("drop the permission table");

    assert_eq!(
        get_media(mgr, pool, Some(principal(uid, false)), id).await,
        StatusCode::FORBIDDEN,
        "the permission lookup errored and the request was served anyway — a \
         database blip must not be readable as a grant"
    );
}
