#![cfg(all(feature = "postgres", feature = "media", feature = "testkit"))]
//! Live integration tests for `MediaManager` against Postgres + an
//! S3-compatible bucket.
//!
//! Both env-var contracts must be set or the tests skip silently:
//!
//! - `DATABASE_URL` — Postgres for the `rustango_media` table
//! - `RUSTANGO_S3_TEST_*` — same shape as `s3_live_*` tests
//!
//! Run:
//! ```text
//! env DATABASE_URL=postgres://rustango:rustango@127.0.0.1:5532/rustango_test \
//!     RUSTANGO_S3_TEST_KEY=rustango \
//!     RUSTANGO_S3_TEST_SECRET=rustango-test-secret \
//!     RUSTANGO_S3_TEST_BUCKET=rustango-test \
//!     RUSTANGO_S3_TEST_ENDPOINT=http://127.0.0.1:9100 \
//!     cargo test -p rustango --test media_live -- --test-threads=1
//! ```

use std::sync::Arc;
use std::time::Duration;

use rustango::media::{MediaManager, MediaStatus, SaveOpts, UploadIntent};
use rustango::storage::s3::{S3Config, S3Storage};
use rustango::storage::{BoxedStorage, StorageRegistry};
use sqlx::PgPool;

const DISK_NAME: &str = "media-live";

async fn maybe_setup() -> Option<MediaManager> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let key = std::env::var("RUSTANGO_S3_TEST_KEY").ok()?;
    let secret = std::env::var("RUSTANGO_S3_TEST_SECRET").ok()?;
    let bucket = std::env::var("RUSTANGO_S3_TEST_BUCKET").ok()?;
    let endpoint = std::env::var("RUSTANGO_S3_TEST_ENDPOINT").ok();
    let region = std::env::var("RUSTANGO_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());

    let pool = PgPool::connect(&url).await.expect("connect Postgres");
    rustango::testkit::migrate_framework(&rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .expect("migrate framework media tables");
    // Wipe between runs so each test sees a clean slate.
    sqlx::query("DELETE FROM rustango_media")
        .execute(&pool)
        .await
        .expect("clear media");

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

#[tokio::test]
async fn save_bytes_inserts_row_and_uploads_object() {
    let Some(manager) = maybe_setup().await else {
        eprintln!("skipping — set DATABASE_URL + RUSTANGO_S3_TEST_*");
        return;
    };

    let payload = b"\x89PNG\r\n\x1a\n-fake-png-bytes-for-test";
    let media = manager
        .save_bytes(SaveOpts {
            disk: DISK_NAME.into(),
            key_prefix: "media-live/save-bytes".into(),
            bytes: payload.to_vec(),
            mime: "image/png".into(),
            original_filename: "test.png".into(),
            uploaded_by_id: Some(99),
            collection_id: None,
            metadata: serde_json::json!({"alt": "test fixture"}),
        })
        .await
        .expect("save_bytes");

    // Row state
    assert!(media.is_ready(), "fresh save should be Ready");
    assert_eq!(media.mime, "image/png");
    assert_eq!(media.size_bytes, payload.len() as i64);
    assert_eq!(media.uploaded_by_id, Some(99));
    assert_eq!(media.metadata["alt"], "test fixture");
    assert!(media.storage_key.starts_with("media-live/save-bytes/"));
    assert!(media.storage_key.ends_with("-test.png"));

    // Storage object actually exists + matches what we wrote.
    let bytes = manager.load_bytes(&media).await.expect("load_bytes");
    assert_eq!(&bytes, payload);

    // CDN URL falls back to backend URL when no CDN configured.
    let url = manager.url(&media).expect("url");
    assert!(url.contains(&media.storage_key));

    // Cleanup.
    manager.purge(&media).await.expect("purge");
}

#[tokio::test]
async fn begin_then_finalize_upload_flips_pending_to_ready() {
    let Some(manager) = maybe_setup().await else {
        return;
    };

    let intent = UploadIntent {
        disk: DISK_NAME.into(),
        key_prefix: "media-live/direct".into(),
        mime: "image/png".into(),
        original_filename: "direct.png".into(),
        size_bytes: 100,
        uploaded_by_id: Some(7),
        collection_id: None,
        ttl: Duration::from_secs(60),
    };

    // Server: begin upload — row goes to Pending, presigned URL returned.
    let ticket = manager.begin_upload(intent).await.expect("begin");
    let pending = manager.get(ticket.media_id).await.expect("get").unwrap();
    assert_eq!(pending.status_enum(), Some(MediaStatus::Pending));
    assert!(ticket.upload_url.starts_with("http"));

    // Browser: PUT directly to the presigned URL.
    let payload = b"-direct-upload-payload-";
    let resp = reqwest::Client::new()
        .put(&ticket.upload_url)
        .header("Content-Type", "image/png")
        .body(payload.to_vec())
        .send()
        .await
        .expect("PUT");
    assert!(
        resp.status().is_success(),
        "PUT failed: {} {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Server: finalize — row flips to Ready since the object now exists.
    let finalized = manager
        .finalize_upload(ticket.media_id)
        .await
        .expect("finalize");
    assert_eq!(finalized.status_enum(), Some(MediaStatus::Ready));

    // Cleanup.
    manager.purge(&finalized).await.expect("purge");
}

#[tokio::test]
async fn finalize_marks_failed_when_object_never_uploaded() {
    let Some(manager) = maybe_setup().await else {
        return;
    };

    let intent = UploadIntent::new(DISK_NAME, "image/png", "ghost.png", 50);
    let ticket = manager.begin_upload(intent).await.expect("begin");

    // Browser never uploads — finalize should detect the absence
    // and mark Failed instead of Ready.
    let finalized = manager
        .finalize_upload(ticket.media_id)
        .await
        .expect("finalize");
    assert_eq!(
        finalized.status_enum(),
        Some(MediaStatus::Failed),
        "missing storage object should flip to Failed, got {:?}",
        finalized.status_enum()
    );
    manager.purge(&finalized).await.ok();
}

#[tokio::test]
async fn delete_soft_then_get_returns_none() {
    let Some(manager) = maybe_setup().await else {
        return;
    };

    let media = manager
        .save_bytes(SaveOpts {
            disk: DISK_NAME.into(),
            key_prefix: "media-live/delete".into(),
            bytes: b"x".to_vec(),
            mime: "text/plain".into(),
            original_filename: "x.txt".into(),
            uploaded_by_id: None,
            collection_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("save");

    let id = match media.id {
        rustango::sql::Auto::Set(v) => v,
        _ => unreachable!(),
    };
    manager.delete(&media).await.expect("soft delete");

    // get() filters out soft-deleted rows.
    assert!(manager.get(id).await.expect("get").is_none());
    // get_including_deleted() still finds it.
    let still = manager
        .get_including_deleted(id)
        .await
        .expect("get incl deleted");
    assert!(still.is_some());
    assert!(still.as_ref().unwrap().deleted_at.is_some());

    // Storage object still there — soft delete preserves it.
    let bytes = manager
        .load_bytes(still.as_ref().unwrap())
        .await
        .expect("load after soft delete");
    assert_eq!(&bytes, b"x");

    // Hard purge clears storage + row.
    manager.purge(still.as_ref().unwrap()).await.expect("purge");
    assert!(manager
        .get_including_deleted(id)
        .await
        .expect("get final")
        .is_none());
}

#[tokio::test]
async fn purge_orphans_clears_old_soft_deleted_rows_and_storage() {
    let Some(manager) = maybe_setup().await else {
        return;
    };

    // Create + soft-delete two rows. Manually backdate them via SQL
    // so they're "old" enough for the sweep.
    let mut ids: Vec<i64> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for i in 0..2 {
        let m = manager
            .save_bytes(SaveOpts {
                disk: DISK_NAME.into(),
                key_prefix: format!("media-live/orphan-{i}"),
                bytes: format!("orphan-{i}").into_bytes(),
                mime: "text/plain".into(),
                original_filename: format!("o{i}.txt"),
                uploaded_by_id: None,
                collection_id: None,
                metadata: serde_json::json!({}),
            })
            .await
            .expect("save");
        let id = match m.id {
            rustango::sql::Auto::Set(v) => v,
            _ => unreachable!(),
        };
        ids.push(id);
        keys.push(m.storage_key.clone());
        manager.delete(&m).await.expect("soft delete");
    }
    // Backdate deleted_at so the sweep picks them up.
    sqlx::query(
        "UPDATE rustango_media SET deleted_at = NOW() - INTERVAL '1 hour'
          WHERE id = ANY($1)",
    )
    .bind(&ids)
    .execute(manager.pool())
    .await
    .expect("backdate");

    let purged = manager
        .purge_orphans(Duration::from_secs(60))
        .await
        .expect("purge_orphans");
    assert!(
        purged >= 2,
        "expected to purge at least 2 orphans, got {purged}"
    );

    // Rows gone.
    for id in &ids {
        assert!(manager.get_including_deleted(*id).await.unwrap().is_none());
    }
    // Storage objects gone too.
    let storage = manager.registry().disk(DISK_NAME).unwrap();
    for k in &keys {
        assert!(!storage.exists(k).await.unwrap(), "key {k} still present");
    }
}

#[tokio::test]
async fn purge_pending_clears_abandoned_uploads() {
    let Some(manager) = maybe_setup().await else {
        return;
    };

    // Create a pending upload but never finalize.
    let intent = UploadIntent::new(DISK_NAME, "image/png", "abandoned.png", 100);
    let ticket = manager.begin_upload(intent).await.expect("begin");

    // Backdate so the sweep picks it up.
    sqlx::query(
        "UPDATE rustango_media SET uploaded_at = NOW() - INTERVAL '1 hour'
          WHERE id = $1",
    )
    .bind(ticket.media_id)
    .execute(manager.pool())
    .await
    .expect("backdate");

    let purged = manager
        .purge_pending(Duration::from_secs(60))
        .await
        .expect("purge_pending");
    assert!(purged >= 1, "expected to purge >=1, got {purged}");

    // Row is gone.
    assert!(manager
        .get_including_deleted(ticket.media_id)
        .await
        .expect("get")
        .is_none());
}
