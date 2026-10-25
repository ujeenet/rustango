//! `purge_pending` holds no database lock while it waits on storage,
//! and a finalize racing it cannot revive a row it is purging.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "media",
    feature = "testkit"
))]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone as _, Utc};
use rustango::core::Column as _;
use rustango::media::{Media, MediaCollection, MediaManager, MediaTag, MediaTagLink, SaveOpts};
use rustango::sql::{Auto, FetcherPool as _, Pool, UpdaterPool as _};
use rustango::storage::{InMemoryStorage, ObjectMeta, Storage, StorageError, StorageRegistry};
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::tri_dialect_test;
use tokio::sync::Notify;

async fn setup(pool: &Pool) {
    // Child-first: tag links hold FKs into the tag table.
    for t in ["rustango_media_tag_links", "rustango_media_tags"] {
        drop_table(pool, t).await;
    }
    fresh_table::<Media>(pool).await;
    fresh_table::<MediaCollection>(pool).await;
    fresh_table::<MediaTag>(pool).await;
    fresh_table::<MediaTagLink>(pool).await;
}

/// Parks a call until the test lets it go.
#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}

impl Gate {
    async fn pass(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }

    async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.notified())
            .await
            .expect("the call never reached storage");
    }
}

/// In-memory storage whose `delete` and `metadata` can be held open.
#[derive(Default)]
struct GatedStorage {
    inner: InMemoryStorage,
    delete: Option<Arc<Gate>>,
    metadata: Option<Arc<Gate>>,
    fail_delete: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Storage for GatedStorage {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
        self.inner.save(key, data).await
    }
    async fn save_with_content_type(
        &self,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> Result<(), StorageError> {
        self.inner
            .save_with_content_type(key, data, content_type)
            .await
    }
    async fn load(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.load(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        if let Some(g) = &self.delete {
            g.pass().await;
        }
        if self.fail_delete.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::Io("bucket unreachable".into()));
        }
        self.inner.delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        self.inner.exists(key).await
    }
    fn url(&self, key: &str) -> Option<String> {
        self.inner.url(key)
    }
    async fn metadata(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        if let Some(g) = &self.metadata {
            g.pass().await;
        }
        self.inner.metadata(key).await
    }
}

fn manager(pool: &Pool, storage: GatedStorage) -> MediaManager {
    let registry = StorageRegistry::new()
        .set("default", Arc::new(storage))
        .with_default("default");
    MediaManager::new_pool(pool.clone(), registry)
}

/// A `pending` upload old enough for any sweep.
async fn seed_pending(mgr: &MediaManager, pool: &Pool) -> i64 {
    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "t/".into(),
            bytes: b"x".to_vec(),
            mime: "text/plain".into(),
            original_filename: "x.txt".into(),
            uploaded_by_id: None,
            collection_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("seed");
    let Auto::Set(id) = m.id else { panic!("no id") };
    Media::objects()
        .where_(Media::id.eq(id))
        .update()
        .set("status", "pending")
        .set(
            "uploaded_at",
            Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap(),
        )
        .execute_pool(pool)
        .await
        .expect("age it");
    id
}

async fn row_count(pool: &Pool, id: i64) -> usize {
    Media::objects()
        .where_(Media::id.eq(id))
        .fetch(pool)
        .await
        .expect("read back")
        .len()
}

/// Another writer gets through while the sweep waits on the bucket.
async fn storage_call_holds_no_lock(pool: &Pool) {
    let gate = Arc::new(Gate::default());
    let mgr = manager(
        pool,
        GatedStorage {
            delete: Some(gate.clone()),
            ..GatedStorage::default()
        },
    );
    let id = seed_pending(&mgr, pool).await;
    let sweep = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.purge_pending(Duration::ZERO).await }
    });
    gate.wait_entered().await;

    let write = Media::objects()
        .where_(Media::id.eq(id))
        .update()
        .set("original_filename", "renamed.txt")
        .execute_pool(pool);
    let r = tokio::time::timeout(Duration::from_secs(3), write).await;
    gate.release.notify_one();
    assert!(
        matches!(r, Ok(Ok(_))),
        "a write blocked or failed while purge_pending waited on storage: {r:?}"
    );
    assert_eq!(sweep.await.unwrap().expect("sweep"), 1);
    assert_eq!(row_count(pool, id).await, 0, "row survived the purge");
}

/// A finalize that read the row before the sweep claimed it does not
/// mark it ready behind the sweep's back.
async fn finalize_cannot_revive_a_claimed_row(pool: &Pool) {
    let (meta_gate, delete_gate) = (Arc::new(Gate::default()), Arc::new(Gate::default()));
    let mgr = manager(
        pool,
        GatedStorage {
            delete: Some(delete_gate.clone()),
            metadata: Some(meta_gate.clone()),
            ..GatedStorage::default()
        },
    );
    let id = seed_pending(&mgr, pool).await;
    let finalize = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.finalize_upload(id).await }
    });
    meta_gate.wait_entered().await;
    let sweep = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.purge_pending(Duration::ZERO).await }
    });
    delete_gate.wait_entered().await;
    meta_gate.release.notify_one();
    let finalized = tokio::time::timeout(Duration::from_secs(10), finalize)
        .await
        .expect("finalize hung")
        .unwrap();
    delete_gate.release.notify_one();
    let purged = sweep.await.unwrap().expect("sweep");

    assert!(
        finalized.is_err(),
        "finalize confirmed a row the sweep was deleting: {finalized:?}"
    );
    assert_eq!(purged, 1);
    assert_eq!(row_count(pool, id).await, 0, "row survived the purge");
}

/// A failed object delete keeps the row, and the next sweep retries it.
async fn storage_error_keeps_the_row_for_the_next_sweep(pool: &Pool) {
    let storage = Arc::new(GatedStorage::default());
    storage
        .fail_delete
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let registry = StorageRegistry::new()
        .set("default", storage.clone())
        .with_default("default");
    let mgr = MediaManager::new_pool(pool.clone(), registry);
    let id = seed_pending(&mgr, pool).await;

    assert!(mgr.purge_pending(Duration::ZERO).await.is_err());
    assert_eq!(
        row_count(pool, id).await,
        1,
        "row dropped on a storage error"
    );

    storage
        .fail_delete
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(mgr.purge_pending(Duration::ZERO).await.expect("retry"), 1);
    assert_eq!(row_count(pool, id).await, 0);
}

tri_dialect_test!(
    setup: setup,
    sqlite: file,
    scenarios: [
        storage_call_holds_no_lock,
        finalize_cannot_revive_a_claimed_row,
        storage_error_keeps_the_row_for_the_next_sweep,
    ]
);
