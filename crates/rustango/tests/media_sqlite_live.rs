#![cfg(all(feature = "sqlite", feature = "media", feature = "testkit"))]
//! Live integration test for the tri-dialect MediaManager on SQLite.
//!
//! v0.38 slice 29 — every MediaManager query is dispatched per
//! backend through `crate::sql::Pool`. PG-specific idioms (`ANY($1)`,
//! `NOW() - INTERVAL`, `DELETE … USING`, `ON CONFLICT DO UPDATE`,
//! `INSERT … RETURNING`) are rewritten portably (`IN (?, ?, …)`,
//! pre-computed cutoffs, subquery rewrites, etc.). This test
//! exercises the SQLite path end-to-end to prove the lift works.

use std::sync::Arc;

use rustango::media::{MediaManager, SaveOpts};
use rustango::sql::Pool;
use rustango::storage::{InMemoryStorage, StorageRegistry};

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

#[tokio::test]
async fn save_get_delete_purge_roundtrip_on_sqlite() {
    let mgr = manager().await;

    // save_bytes → INSERT … RETURNING (or LAST_INSERT_ID() on MySQL).
    let media = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "users/".into(),
            bytes: b"hello world".to_vec(),
            mime: "text/plain".into(),
            original_filename: "hello.txt".into(),
            uploaded_by_id: Some(42),
            collection_id: None,
            metadata: serde_json::json!({"source": "test"}),
        })
        .await
        .expect("save_bytes");
    let id = match media.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!("expected Auto::Set after save"),
    };
    assert_eq!(media.original_filename, "hello.txt");
    assert_eq!(media.size_bytes, 11);
    assert_eq!(media.uploaded_by_id, Some(42));
    assert_eq!(media.metadata, serde_json::json!({"source": "test"}));

    // get → SELECT with id = ?
    let fetched = mgr.get(id).await.expect("get").expect("row");
    assert_eq!(fetched.original_filename, "hello.txt");

    // delete → UPDATE SET deleted_at = ? (Utc::now() bound from Rust)
    mgr.delete(&fetched).await.expect("delete");
    assert!(
        mgr.get(id).await.expect("get").is_none(),
        "soft-deleted media should not be visible to get()"
    );
    assert!(
        mgr.get_including_deleted(id)
            .await
            .expect("get_including_deleted")
            .is_some(),
        "soft-deleted row still readable via get_including_deleted"
    );

    // purge → DELETE
    mgr.purge(&fetched).await.expect("purge");
    assert!(
        mgr.get_including_deleted(id)
            .await
            .expect("get_including_deleted")
            .is_none(),
        "hard-deleted row should be gone"
    );
}

#[tokio::test]
async fn collection_crud_and_list_in_collection_on_sqlite() {
    let mgr = manager().await;

    // create_collection → INSERT … RETURNING
    let folder = mgr
        .create_collection("Launch", "launch", None, "2026 launch assets")
        .await
        .expect("create_collection");
    let folder_id = match folder.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!("expected Auto::Set"),
    };
    assert_eq!(folder.slug, "launch");
    assert!(folder.parent_id.is_none());

    // Nested sub-folder.
    let sub = mgr
        .create_collection("Hero", "hero", Some(folder_id), "")
        .await
        .expect("create_collection sub");
    let _sub_id = match sub.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!(),
    };

    // get_collection / get_collection_by_slug
    let got = mgr
        .get_collection(folder_id)
        .await
        .expect("get_collection")
        .expect("row");
    assert_eq!(got.slug, "launch");
    let by_slug = mgr
        .get_collection_by_slug("launch")
        .await
        .expect("get_collection_by_slug")
        .expect("row");
    assert_eq!(by_slug.name, "Launch");

    // list_collections — ordered by "parent_id IS NULL DESC, parent_id, name"
    // so the root collection comes first.
    let all = mgr.list_collections().await.expect("list_collections");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].slug, "launch", "root collection ordered first");
    assert_eq!(all[1].slug, "hero");

    // collection_path walks the parent chain.
    let path = mgr.collection_path(folder_id).await.expect("path");
    assert_eq!(path, "launch");

    // Add a Media into the sub-collection.
    let _ = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: String::new(),
            bytes: b"data".to_vec(),
            mime: "image/png".into(),
            original_filename: "hero.png".into(),
            uploaded_by_id: None,
            collection_id: Some(folder_id),
            metadata: serde_json::Value::Object(Default::default()),
        })
        .await
        .expect("save_bytes in collection");

    // list_in_collection — exercises the `IN (?, …)` expansion that
    // replaced the PG-only `ANY($1)`.
    let in_folder = mgr
        .list_in_collection(folder_id, false)
        .await
        .expect("list_in_collection");
    assert_eq!(in_folder.len(), 1);
    assert_eq!(in_folder[0].original_filename, "hero.png");

    // delete_collection — orphans the media (collection_id ← NULL) +
    // soft-deletes the collection row (deleted_at ← Utc::now()).
    mgr.delete_collection(folder_id)
        .await
        .expect("delete_collection");
    assert!(
        mgr.get_collection(folder_id)
            .await
            .expect("get_collection")
            .is_none(),
        "collection should be soft-deleted"
    );
}

#[tokio::test]
async fn tag_lifecycle_on_sqlite() {
    let mgr = manager().await;
    let media = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: String::new(),
            bytes: vec![1, 2, 3],
            mime: "application/octet-stream".into(),
            original_filename: "blob.bin".into(),
            uploaded_by_id: None,
            collection_id: None,
            metadata: serde_json::Value::Object(Default::default()),
        })
        .await
        .expect("save_bytes");
    let media_id = match media.id {
        rustango::sql::Auto::Set(v) => v,
        _ => panic!(),
    };

    // ensure_tag → INSERT … ON CONFLICT DO UPDATE … RETURNING (PG/SQLite),
    // INSERT … ON DUPLICATE KEY UPDATE + SELECT (MySQL).
    let t1 = mgr.ensure_tag("featured").await.expect("ensure_tag");
    assert_eq!(t1.slug, "featured");
    // Calling again returns the same row (idempotent).
    let t1_again = mgr.ensure_tag("featured").await.expect("ensure_tag");
    assert_eq!(t1.id.get().copied(), t1_again.id.get().copied());

    // tag → INSERT IGNORE (MySQL) or ON CONFLICT DO NOTHING (PG/SQLite)
    mgr.tag(media_id, &["featured", "approved"])
        .await
        .expect("tag");
    let tags = mgr.tags_for(media_id).await.expect("tags_for");
    assert_eq!(tags.len(), 2);
    let slugs: Vec<&str> = tags.iter().map(|t| t.slug.as_str()).collect();
    assert!(slugs.contains(&"approved"));
    assert!(slugs.contains(&"featured"));

    // list_with_tag — JOIN with the tag slug.
    let listed = mgr
        .list_with_tag("featured", 10, 0)
        .await
        .expect("list_with_tag");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].id.get().copied(),
        Some(media_id),
        "listed media id should match"
    );

    // untag — exercises the subquery rewrite (replaces PG-only
    // `DELETE … USING …`).
    mgr.untag(media_id, "featured").await.expect("untag");
    let tags = mgr.tags_for(media_id).await.expect("tags_for");
    let slugs: Vec<&str> = tags.iter().map(|t| t.slug.as_str()).collect();
    assert!(!slugs.contains(&"featured"));
    assert!(slugs.contains(&"approved"));

    // set_tags replaces the entire tag set.
    mgr.set_tags(media_id, &["new1", "new2", "new3"])
        .await
        .expect("set_tags");
    let tags = mgr.tags_for(media_id).await.expect("tags_for");
    assert_eq!(tags.len(), 3);

    // popular_tags — GROUP BY + ORDER BY use_count DESC.
    let popular = mgr.popular_tags(10).await.expect("popular_tags");
    assert!(popular.len() >= 3, "got: {:?}", popular);
    // Tags applied to our one media row should all have count = 1.
    for (tag, count) in &popular {
        if ["new1", "new2", "new3"].contains(&tag.slug.as_str()) {
            assert_eq!(*count, 1, "tag {} should have count 1", tag.slug);
        }
    }
}

/// Records the content type each `save_with_content_type` hands the backend.
#[derive(Default)]
struct RecordsType(InMemoryStorage, std::sync::Mutex<Vec<Option<String>>>);

#[rustango::storage::async_trait]
impl rustango::storage::Storage for RecordsType {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), rustango::storage::StorageError> {
        self.save_with_content_type(key, data, None).await
    }
    async fn save_with_content_type(
        &self,
        key: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> Result<(), rustango::storage::StorageError> {
        self.1.lock().unwrap().push(content_type.map(str::to_owned));
        self.0.save(key, data).await
    }
    async fn load(&self, key: &str) -> Result<Vec<u8>, rustango::storage::StorageError> {
        self.0.load(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), rustango::storage::StorageError> {
        self.0.delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool, rustango::storage::StorageError> {
        self.0.exists(key).await
    }
    fn url(&self, key: &str) -> Option<String> {
        self.0.url(key)
    }
}

/// #1904 — the MIME reaches storage, but a client-declared active type
/// (it would run on the bucket origin) is stored as octet-stream.
#[tokio::test]
async fn save_bytes_hands_a_safe_mime_to_storage() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite connect");
    let pool = Pool::Sqlite(pool);
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate");
    let disk = Arc::new(RecordsType::default());
    let registry = StorageRegistry::new()
        .set("default", disk.clone())
        .with_default("default");
    let mgr = MediaManager::new_pool(pool, registry);
    let mimes = [
        "image/png",
        "text/html",
        "image/svg+xml",
        "Application/XHTML+XML",
        "text/javascript; charset=utf-8",
        "application/unknown",
        "nonsense",
    ];
    for mime in mimes {
        let media = mgr
            .save_bytes(SaveOpts {
                disk: "default".into(),
                key_prefix: "u/".into(),
                bytes: b"<script>alert(1)</script>".to_vec(),
                mime: mime.into(),
                original_filename: "f.bin".into(),
                uploaded_by_id: None,
                collection_id: None,
                metadata: serde_json::json!({}),
            })
            .await
            .expect("save_bytes");
        assert_eq!(media.mime, mime, "the row keeps the declared type");
    }
    let octet = Some("application/octet-stream".to_owned());
    let mut want = vec![Some("image/png".to_owned())];
    want.extend(std::iter::repeat_n(octet, mimes.len() - 1));
    assert_eq!(*disk.1.lock().unwrap(), want);
}

/// A bucket that can presign, like S3, but does not enforce the
/// signature, so a test can play a client that ignores it.
#[derive(Default)]
struct FakeBucket {
    files: std::sync::Mutex<std::collections::HashMap<String, (Vec<u8>, Option<String>)>>,
    signed: std::sync::Mutex<Vec<rustango::storage::PutConditions>>,
}

impl FakeBucket {
    fn put(&self, key: &str, body: &[u8], ct: &str) {
        let v = (body.to_vec(), Some(ct.to_owned()));
        self.files.lock().unwrap().insert(key.to_owned(), v);
    }
    fn has(&self, key: &str) -> bool {
        self.files.lock().unwrap().contains_key(key)
    }
}

#[rustango::storage::async_trait]
impl rustango::storage::Storage for FakeBucket {
    async fn save(&self, key: &str, data: &[u8]) -> Result<(), rustango::storage::StorageError> {
        self.save_with_content_type(key, data, None).await
    }
    async fn save_with_content_type(
        &self,
        key: &str,
        data: &[u8],
        ct: Option<&str>,
    ) -> Result<(), rustango::storage::StorageError> {
        let v = (data.to_vec(), ct.map(str::to_owned));
        self.files.lock().unwrap().insert(key.to_owned(), v);
        Ok(())
    }
    async fn load(&self, key: &str) -> Result<Vec<u8>, rustango::storage::StorageError> {
        let files = self.files.lock().unwrap();
        files
            .get(key)
            .map(|v| v.0.clone())
            .ok_or_else(|| rustango::storage::StorageError::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<(), rustango::storage::StorageError> {
        self.files.lock().unwrap().remove(key);
        Ok(())
    }
    async fn exists(&self, key: &str) -> Result<bool, rustango::storage::StorageError> {
        Ok(self.has(key))
    }
    fn url(&self, _key: &str) -> Option<String> {
        None
    }
    async fn presigned_put_url(
        &self,
        key: &str,
        _ttl: std::time::Duration,
        put: &rustango::storage::PutConditions,
    ) -> Option<String> {
        self.signed.lock().unwrap().push(put.clone());
        Some(format!("mem://{key}"))
    }
    async fn metadata(
        &self,
        key: &str,
    ) -> Result<Option<rustango::storage::ObjectMeta>, rustango::storage::StorageError> {
        let files = self.files.lock().unwrap();
        Ok(files
            .get(key)
            .map(|(b, ct)| rustango::storage::ObjectMeta::new(b.len() as u64, ct.clone())))
    }
}

async fn bucket_manager() -> (MediaManager, Arc<FakeBucket>, sqlx::SqlitePool) {
    let sq = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite connect");
    let pool = Pool::Sqlite(sq.clone());
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate");
    let disk = Arc::new(FakeBucket::default());
    let registry = StorageRegistry::new()
        .set("default", disk.clone())
        .with_default("default");
    (MediaManager::new_pool(pool, registry), disk, sq)
}

/// #2057: an active MIME is signed, and reported, as octet-stream.
#[tokio::test]
async fn begin_upload_signs_a_safe_type_and_the_declared_size() {
    let (mgr, disk, _) = bucket_manager().await;
    let intent = rustango::media::UploadIntent::new("default", "image/svg+xml", "x.svg", 10);
    let ticket = mgr.begin_upload(intent).await.expect("begin");
    assert_eq!(ticket.content_type, "application/octet-stream");
    let want = rustango::storage::PutConditions::new()
        .content_type("application/octet-stream")
        .content_length(10)
        .create_only();
    assert_eq!(*disk.signed.lock().unwrap(), vec![want]);
    assert_eq!(
        ticket.headers.get("if-none-match").map(String::as_str),
        Some("*")
    );
    let neg = rustango::media::UploadIntent::new("default", "image/png", "x.png", -1);
    assert!(mgr.begin_upload(neg).await.is_err(), "negative size signed");
}

/// #1851: finalize checks the object, not the client's claim.
#[tokio::test]
async fn finalize_refuses_an_object_of_another_size_or_type() {
    use rustango::media::{MediaStatus, UploadIntent};
    let (mgr, disk, _) = bucket_manager().await;
    let cases: [(&[u8], &str, MediaStatus); 3] = [
        (&[b'x'; 20], "image/png", MediaStatus::Failed),
        (b"0123456789", "text/html", MediaStatus::Failed),
        (b"0123456789", "image/png", MediaStatus::Ready),
    ];
    for (body, ct, want) in cases {
        let intent = UploadIntent::new("default", "image/png", "a.png", 10);
        let t = mgr.begin_upload(intent).await.expect("begin");
        disk.put(&t.storage_key, body, ct);
        let m = mgr.finalize_upload(t.media_id).await.expect("finalize");
        assert_eq!(m.status_enum(), Some(want), "{} bytes as {ct}", body.len());
        let stored = mgr.get(t.media_id).await.unwrap().unwrap();
        assert_eq!(stored.status_enum(), Some(want));
        assert_eq!(disk.has(&t.storage_key), want == MediaStatus::Ready);
    }
}

/// #1905: a failed row insert must not leave an object no sweep finds.
#[tokio::test]
async fn save_bytes_removes_the_object_when_the_row_insert_fails() {
    let (mgr, disk, sq) = bucket_manager().await;
    sqlx::query("DROP TABLE rustango_media")
        .execute(&sq)
        .await
        .expect("drop");
    let opts = SaveOpts {
        disk: "default".into(),
        key_prefix: "u/".into(),
        bytes: b"data".to_vec(),
        mime: "image/png".into(),
        original_filename: "a.png".into(),
        uploaded_by_id: None,
        collection_id: None,
        metadata: serde_json::json!({}),
    };
    assert!(mgr.save_bytes(opts).await.is_err());
    assert!(disk.files.lock().unwrap().is_empty(), "object left behind");
}

/// A Ready row: a second finalize must not delete its object.
#[tokio::test]
async fn finalize_on_a_ready_row_is_a_no_op() {
    use rustango::media::{MediaStatus, UploadIntent};
    let (mgr, disk, _) = bucket_manager().await;
    let t = mgr
        .begin_upload(UploadIntent::new("default", "image/png", "a.png", 10))
        .await
        .expect("begin");
    disk.put(&t.storage_key, b"0123456789", "image/png");
    let m = mgr.finalize_upload(t.media_id).await.expect("finalize");
    assert_eq!(m.status_enum(), Some(MediaStatus::Ready));
    // A Change-only caller swaps in a mismatched body and re-finalizes.
    disk.put(&t.storage_key, b"too long for the row", "image/png");
    let again = mgr.finalize_upload(t.media_id).await.expect("refinalize");
    assert_eq!(again.status_enum(), Some(MediaStatus::Ready));
    let stored = mgr.get(t.media_id).await.unwrap().unwrap();
    assert_eq!(stored.status_enum(), Some(MediaStatus::Ready));
    assert!(disk.has(&t.storage_key), "a Ready row lost its object");
}

/// A Failed row stays Failed, even once a matching object lands.
#[tokio::test]
async fn finalize_on_a_failed_row_stays_failed() {
    use rustango::media::{MediaStatus, UploadIntent};
    let (mgr, disk, _) = bucket_manager().await;
    let t = mgr
        .begin_upload(UploadIntent::new("default", "image/png", "a.png", 10))
        .await
        .expect("begin");
    let m = mgr.finalize_upload(t.media_id).await.expect("finalize");
    assert_eq!(m.status_enum(), Some(MediaStatus::Failed), "no object yet");
    disk.put(&t.storage_key, b"0123456789", "image/png");
    let again = mgr.finalize_upload(t.media_id).await.expect("refinalize");
    assert_eq!(again.status_enum(), Some(MediaStatus::Failed));
    let stored = mgr.get(t.media_id).await.unwrap().unwrap();
    assert_eq!(stored.status_enum(), Some(MediaStatus::Failed));
}

/// Purging Pending and Failed rows takes their objects too; a missing
/// key is fine, and a Ready row is kept.
#[tokio::test]
async fn purge_pending_deletes_unconfirmed_objects() {
    use rustango::media::{MediaStatus, UploadIntent};
    let (mgr, disk, _) = bucket_manager().await;
    let begin = |name: &'static str| {
        let mgr = &mgr;
        async move {
            mgr.begin_upload(UploadIntent::new("default", "image/png", name, 10))
                .await
                .expect("begin")
        }
    };
    let (pending, failed, empty, ready) = (
        begin("p.png").await,
        begin("f.png").await,
        begin("e.png").await,
        begin("r.png").await,
    );
    disk.put(&pending.storage_key, b"0123456789", "image/png");
    let f = mgr
        .finalize_upload(failed.media_id)
        .await
        .expect("finalize");
    assert_eq!(f.status_enum(), Some(MediaStatus::Failed));
    // The PUT lands after finalize already gave up.
    disk.put(&failed.storage_key, b"0123456789", "image/png");
    disk.put(&ready.storage_key, b"0123456789", "image/png");
    mgr.finalize_upload(ready.media_id).await.expect("finalize");
    mgr.tag(pending.media_id, &["t"]).await.expect("tag");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let purged = mgr
        .purge_pending(std::time::Duration::ZERO)
        .await
        .expect("purge");
    assert_eq!(purged, 3);
    for t in [&pending, &failed, &empty] {
        assert!(mgr.get(t.media_id).await.unwrap().is_none(), "row kept");
        assert!(!disk.has(&t.storage_key), "object left: {}", t.storage_key);
    }
    assert!(mgr.tags_for(pending.media_id).await.unwrap().is_empty());
    assert!(mgr.get(ready.media_id).await.unwrap().is_some());
    assert!(disk.has(&ready.storage_key), "a Ready object was purged");
}
