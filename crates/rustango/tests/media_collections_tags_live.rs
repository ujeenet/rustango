#![cfg(feature = "postgres")]
//! Live integration tests for `MediaCollection` + `MediaTag` + the
//! axum router. Same env-var contract as `media_live.rs` — skips
//! silently when DATABASE_URL or RUSTANGO_S3_TEST_* are unset.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::media::router::media_router;
use rustango::media::{ensure_all_tables, MediaManager, SaveOpts, UploadIntent};
use rustango::storage::s3::{S3Config, S3Storage};
use rustango::storage::{BoxedStorage, StorageRegistry};
use sqlx::PgPool;
use tower::ServiceExt;

const DISK_NAME: &str = "media-collections-live";

async fn maybe_setup() -> Option<MediaManager> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let key = std::env::var("RUSTANGO_S3_TEST_KEY").ok()?;
    let secret = std::env::var("RUSTANGO_S3_TEST_SECRET").ok()?;
    let bucket = std::env::var("RUSTANGO_S3_TEST_BUCKET").ok()?;
    let endpoint = std::env::var("RUSTANGO_S3_TEST_ENDPOINT").ok();
    let region = std::env::var("RUSTANGO_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());

    let pool = PgPool::connect(&url).await.expect("connect Postgres");
    ensure_all_tables(&pool).await.expect("ensure_all_tables");
    // Wipe between runs — every test gets a clean slate.
    sqlx::query("DELETE FROM rustango_media_tag_links")
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM rustango_media")
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM rustango_media_tags")
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM rustango_media_collections")
        .execute(&pool)
        .await
        .ok();

    let storage: BoxedStorage = Arc::new(S3Storage::new(S3Config {
        bucket,
        region,
        endpoint: endpoint.clone(),
        access_key_id: key,
        secret_access_key: secret,
        path_style: endpoint.is_some(),
    }));
    let registry = StorageRegistry::new()
        .set(DISK_NAME, storage)
        .with_default(DISK_NAME);
    Some(MediaManager::new(pool, registry))
}

fn save_opts(name: &str) -> SaveOpts {
    SaveOpts {
        disk: DISK_NAME.into(),
        key_prefix: "collections-live".into(),
        bytes: format!("body-of-{name}").into_bytes(),
        mime: "image/png".into(),
        original_filename: format!("{name}.png"),
        uploaded_by_id: None,
        collection_id: None,
        metadata: serde_json::json!({}),
    }
}

// =====================================================================
// Collections
// =====================================================================

#[tokio::test]
async fn create_then_get_collection_round_trips() {
    let Some(manager) = maybe_setup().await else {
        eprintln!("skipping — set DATABASE_URL + RUSTANGO_S3_TEST_*");
        return;
    };
    let c = manager
        .create_collection("Products 2026", "products-2026", None, "")
        .await
        .expect("create");
    let id = match c.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let by_id = manager.get_collection(id).await.unwrap().unwrap();
    assert_eq!(by_id.slug, "products-2026");
    let by_slug = manager
        .get_collection_by_slug("products-2026")
        .await
        .unwrap()
        .unwrap();
    let by_slug_id = match by_slug.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    assert_eq!(by_slug_id, id);
}

#[tokio::test]
async fn collection_path_walks_parent_chain() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let root = manager
        .create_collection("Products", "products", None, "")
        .await
        .unwrap();
    let root_id = match root.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let mid = manager
        .create_collection("2026", "2026", Some(root_id), "")
        .await
        .unwrap();
    let mid_id = match mid.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let leaf = manager
        .create_collection("Launch", "launch", Some(mid_id), "")
        .await
        .unwrap();
    let leaf_id = match leaf.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let path = manager.collection_path(leaf_id).await.unwrap();
    assert_eq!(path, "products/2026/launch");
}

#[tokio::test]
async fn list_in_collection_recursive_descends_subfolders() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let root = manager.create_collection("R", "r", None, "").await.unwrap();
    let root_id = match root.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let sub = manager
        .create_collection("S", "s", Some(root_id), "")
        .await
        .unwrap();
    let sub_id = match sub.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    // One file in root, one in sub.
    let m_root = manager
        .save_bytes(SaveOpts {
            collection_id: Some(root_id),
            ..save_opts("root-file")
        })
        .await
        .unwrap();
    let m_sub = manager
        .save_bytes(SaveOpts {
            collection_id: Some(sub_id),
            ..save_opts("sub-file")
        })
        .await
        .unwrap();

    // Non-recursive — only root file.
    let only_root = manager.list_in_collection(root_id, false).await.unwrap();
    assert_eq!(only_root.len(), 1);

    // Recursive — both.
    let both = manager.list_in_collection(root_id, true).await.unwrap();
    assert_eq!(both.len(), 2);

    // Cleanup.
    manager.purge(&m_root).await.ok();
    manager.purge(&m_sub).await.ok();
}

#[tokio::test]
async fn delete_collection_orphans_media_not_storage() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let c = manager.create_collection("X", "x", None, "").await.unwrap();
    let cid = match c.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let m = manager
        .save_bytes(SaveOpts {
            collection_id: Some(cid),
            ..save_opts("orphan-me")
        })
        .await
        .unwrap();
    let mid = match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    manager.delete_collection(cid).await.unwrap();

    // Collection gone (soft-deleted).
    assert!(manager.get_collection(cid).await.unwrap().is_none());
    // Media still queryable, but collection_id cleared.
    let still = manager.get(mid).await.unwrap().unwrap();
    assert_eq!(
        still.collection_id, None,
        "Media collection_id should be NULL after orphan"
    );
    // Storage object still present.
    let bytes = manager.load_bytes(&still).await.unwrap();
    assert!(!bytes.is_empty());

    manager.purge(&still).await.ok();
}

#[tokio::test]
async fn move_to_collection_updates_fk() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let a = manager.create_collection("A", "a", None, "").await.unwrap();
    let b = manager.create_collection("B", "b", None, "").await.unwrap();
    let aid = match a.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let bid = match b.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let m = manager
        .save_bytes(SaveOpts {
            collection_id: Some(aid),
            ..save_opts("move-me")
        })
        .await
        .unwrap();
    let mid = match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    manager.move_to_collection(mid, Some(bid)).await.unwrap();
    let updated = manager.get(mid).await.unwrap().unwrap();
    assert_eq!(updated.collection_id, Some(bid));

    // Move to root.
    manager.move_to_collection(mid, None).await.unwrap();
    let updated = manager.get(mid).await.unwrap().unwrap();
    assert_eq!(updated.collection_id, None);

    manager.purge(&updated).await.ok();
}

// =====================================================================
// Tags
// =====================================================================

#[tokio::test]
async fn tag_then_tags_for_round_trips() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let m = manager.save_bytes(save_opts("tagged")).await.unwrap();
    let mid = match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    manager
        .tag(mid, &["featured", "homepage", "approved"])
        .await
        .unwrap();
    let mut slugs: Vec<String> = manager
        .tags_for(mid)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    slugs.sort();
    assert_eq!(
        slugs,
        vec![
            "approved".to_owned(),
            "featured".to_owned(),
            "homepage".to_owned()
        ]
    );

    // Idempotent — re-tagging the same slug doesn't duplicate.
    manager.tag(mid, &["featured"]).await.unwrap();
    assert_eq!(manager.tags_for(mid).await.unwrap().len(), 3);

    manager.purge(&m).await.ok();
}

#[tokio::test]
async fn untag_removes_one_keeps_others() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let m = manager.save_bytes(save_opts("untag-me")).await.unwrap();
    let mid = match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    manager.tag(mid, &["a", "b", "c"]).await.unwrap();
    manager.untag(mid, "b").await.unwrap();
    let mut slugs: Vec<String> = manager
        .tags_for(mid)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    slugs.sort();
    assert_eq!(slugs, vec!["a".to_owned(), "c".to_owned()]);

    manager.purge(&m).await.ok();
}

#[tokio::test]
async fn set_tags_replaces_entire_set() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let m = manager.save_bytes(save_opts("set-tags")).await.unwrap();
    let mid = match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    manager.tag(mid, &["old1", "old2"]).await.unwrap();
    manager
        .set_tags(mid, &["new1", "new2", "new3"])
        .await
        .unwrap();
    let mut slugs: Vec<String> = manager
        .tags_for(mid)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    slugs.sort();
    assert_eq!(
        slugs,
        vec!["new1".to_owned(), "new2".to_owned(), "new3".to_owned()]
    );

    manager.purge(&m).await.ok();
}

#[tokio::test]
async fn list_with_tag_returns_matching_media() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let a = manager.save_bytes(save_opts("a")).await.unwrap();
    let b = manager.save_bytes(save_opts("b")).await.unwrap();
    let c = manager.save_bytes(save_opts("c")).await.unwrap();
    let aid = match a.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let bid = match b.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let cid = match c.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    manager.tag(aid, &["featured"]).await.unwrap();
    manager.tag(bid, &["featured", "draft"]).await.unwrap();
    manager.tag(cid, &["draft"]).await.unwrap();

    let featured = manager.list_with_tag("featured", 10, 0).await.unwrap();
    assert_eq!(featured.len(), 2);
    let draft = manager.list_with_tag("draft", 10, 0).await.unwrap();
    assert_eq!(draft.len(), 2);

    manager.purge(&a).await.ok();
    manager.purge(&b).await.ok();
    manager.purge(&c).await.ok();
}

#[tokio::test]
async fn popular_tags_orders_by_use_count() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let m1 = manager.save_bytes(save_opts("m1")).await.unwrap();
    let m2 = manager.save_bytes(save_opts("m2")).await.unwrap();
    let m3 = manager.save_bytes(save_opts("m3")).await.unwrap();
    let m1id = match m1.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let m2id = match m2.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let m3id = match m3.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    // popular should be top: 3, mid: 2, niche: 1.
    manager
        .tag(m1id, &["popular", "mid", "niche"])
        .await
        .unwrap();
    manager.tag(m2id, &["popular", "mid"]).await.unwrap();
    manager.tag(m3id, &["popular"]).await.unwrap();

    let top = manager.popular_tags(3).await.unwrap();
    let order: Vec<String> = top.into_iter().map(|(t, _)| t.slug).collect();
    assert_eq!(
        order,
        vec!["popular".to_owned(), "mid".to_owned(), "niche".to_owned()]
    );

    manager.purge(&m1).await.ok();
    manager.purge(&m2).await.ok();
    manager.purge(&m3).await.ok();
}

// =====================================================================
// Router
// =====================================================================

#[tokio::test]
async fn router_get_media_returns_full_response() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let m = manager.save_bytes(save_opts("via-router")).await.unwrap();
    let mid = match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    manager.tag(mid, &["api"]).await.unwrap();

    let app = media_router(manager.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/media/{mid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["id"], mid);
    assert_eq!(v["mime"], "image/png");
    assert_eq!(v["status"], "ready");
    assert_eq!(v["tags"], serde_json::json!(["api"]));
    // url + presigned_url should be present (S3 backend).
    assert!(v["url"].is_string());
    assert!(v["presigned_url"].is_string());

    manager.purge(&m).await.ok();
}

#[tokio::test]
async fn router_create_collection_then_list_and_get() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let app = media_router(manager.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/collections")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "Hero Images",
                        "slug": "hero-images",
                        "description": "front-page heroes"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let cid = v["id"].as_i64().unwrap();
    assert_eq!(v["slug"], "hero-images");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/collections/{cid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["slug"], "hero-images");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/collections")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn router_begin_then_finalize_upload_via_axum() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let app = media_router(manager.clone());

    // 1. POST /uploads/begin
    let begin = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/uploads/begin")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "disk": DISK_NAME,
                        "key_prefix": "collections-live/router",
                        "mime": "image/png",
                        "original_filename": "router.png",
                        "size_bytes": 100,
                        "ttl_secs": 60
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(begin.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(begin.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let media_id = v["media_id"].as_i64().unwrap();
    let upload_url = v["upload_url"].as_str().unwrap().to_owned();

    // 2. Browser PUTs to the presigned URL.
    let put = reqwest::Client::new()
        .put(&upload_url)
        .header("Content-Type", "image/png")
        .body(b"-router-payload-".to_vec())
        .send()
        .await
        .expect("PUT");
    assert!(put.status().is_success(), "PUT failed: {}", put.status());

    // 3. POST /uploads/{id}/finalize
    let fin = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/uploads/{media_id}/finalize"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fin.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(fin.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["status"], "ready");

    // Cleanup directly via manager.
    let m = manager.get(media_id).await.unwrap().unwrap();
    manager.purge(&m).await.ok();
}

#[tokio::test]
async fn router_set_tags_and_query_via_tag_endpoint() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let m = manager.save_bytes(save_opts("router-tags")).await.unwrap();
    let mid = match m.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let app = media_router(manager.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/media/{mid}/tags"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "slugs": ["router-set", "live"]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Now GET /tags/router-set/media
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/tags/router-set/media")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], mid);

    manager.purge(&m).await.ok();
}

#[tokio::test]
async fn router_collection_contents_with_recursive_query() {
    let Some(manager) = maybe_setup().await else {
        return;
    };
    let root = manager.create_collection("R", "r", None, "").await.unwrap();
    let root_id = match root.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    let sub = manager
        .create_collection("S", "s", Some(root_id), "")
        .await
        .unwrap();
    let sub_id = match sub.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };

    let _root_m = manager
        .save_bytes(SaveOpts {
            collection_id: Some(root_id),
            ..save_opts("rf")
        })
        .await
        .unwrap();
    let _sub_m = manager
        .save_bytes(SaveOpts {
            collection_id: Some(sub_id),
            ..save_opts("sf")
        })
        .await
        .unwrap();

    let app = media_router(manager.clone());

    // Non-recursive
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/collections/{root_id}/contents"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);

    // Recursive
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/collections/{root_id}/contents?recursive=true"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);

    // Cleanup all media in this test.
    sqlx::query("DELETE FROM rustango_media")
        .execute(manager.pool())
        .await
        .ok();
}

#[tokio::test]
async fn ensure_all_tables_works_against_running_db() {
    let Some(_manager) = maybe_setup().await else {
        return;
    };
    // The setup helper already calls ensure_all_tables; a second
    // call must be a no-op (idempotent DDL).
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    rustango::media::ensure_all_tables(&pool)
        .await
        .expect("ensure_all_tables idempotent");
    rustango::media::ensure_all_tables(&pool)
        .await
        .expect("ensure_all_tables called twice");
}
