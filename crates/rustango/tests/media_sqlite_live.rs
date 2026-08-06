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
