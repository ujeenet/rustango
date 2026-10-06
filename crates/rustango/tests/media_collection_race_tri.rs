//! `delete_collection` and `create_collection` agree on a subtree even
//! when they run at once (#1573): no live child under a deleted parent.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "media",
    feature = "testkit"
))]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rustango::core::Column as _;
use rustango::media::{Media, MediaCollection, MediaError, MediaManager};
use rustango::sql::{transaction_pool, update_tx, Auto, FetcherTx as _, Pool};
use rustango::storage::{InMemoryStorage, StorageRegistry};
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    // Child-first: tag links hold FKs into the media table.
    for t in ["rustango_media_tag_links", "rustango_media_tags"] {
        drop_table(pool, t).await;
    }
    fresh_table::<Media>(pool).await;
    fresh_table::<MediaCollection>(pool).await;
}

fn manager(pool: &Pool) -> MediaManager {
    let registry = StorageRegistry::new()
        .set("default", Arc::new(InMemoryStorage::new()))
        .with_default("default");
    MediaManager::new_pool(pool.clone(), registry)
}

async fn create(mgr: &MediaManager, slug: &str, parent: Option<i64>) -> i64 {
    let c = mgr
        .create_collection(slug, slug, parent, "")
        .await
        .expect("create");
    let Auto::Set(id) = c.id else { panic!("no id") };
    id
}

fn is_not_found(r: &Result<MediaCollection, MediaError>) -> bool {
    matches!(r, Err(MediaError::Other(m)) if m.contains("not found"))
}

/// A missing or deleted parent is refused, and a delete takes the subtree.
async fn dead_parent_is_refused(pool: &Pool) {
    let mgr = manager(pool);
    let root = create(&mgr, "seq-root", None).await;
    let mid = create(&mgr, "seq-mid", Some(root)).await;
    let leaf = create(&mgr, "seq-leaf", Some(mid)).await;
    mgr.delete_collection(root).await.expect("delete");
    for id in [root, mid, leaf] {
        assert!(mgr.get_collection(id).await.unwrap().is_none(), "{id} live");
    }
    let r = mgr.create_collection("x", "seq-x", Some(mid), "").await;
    assert!(is_not_found(&r), "{r:?}");
    let r = mgr
        .create_collection("y", "seq-y", Some(i64::MAX), "")
        .await;
    assert!(is_not_found(&r), "{r:?}");
}

/// A child committed while the delete waits on its parent is deleted too.
async fn delete_takes_a_child_created_meanwhile(pool: &Pool) {
    if pool.dialect().name() == "sqlite" {
        return; // One writer at a time; `dead_parent_is_refused` covers it.
    }
    let mgr = manager(pool);
    let root = create(&mgr, "race-root", None).await;
    let mid = create(&mgr, "race-mid", Some(root)).await;

    // What `create_collection` does: lock the parent, insert, commit.
    let mut tx = transaction_pool(pool).await.expect("begin");
    let locked = MediaCollection::objects()
        .where_(MediaCollection::id.eq(mid))
        .select_for_update()
        .fetch_tx(&mut tx)
        .await
        .expect("lock parent");
    assert_eq!(locked.len(), 1);
    let mut child = MediaCollection {
        id: Auto::Unset,
        name: "race-child".into(),
        slug: "race-child".into(),
        parent_id: Some(mid),
        description: String::new(),
        created_at: Auto::Unset,
        deleted_at: None,
    };
    child.insert_tx(&mut tx).await.expect("insert child");
    let Auto::Set(child_id) = child.id else {
        panic!("no id")
    };

    let deleting = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.delete_collection(root).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !deleting.is_finished(),
        "the delete did not wait for the lock"
    );
    tx.commit().await.expect("commit");
    deleting.await.unwrap().expect("delete");

    assert!(
        mgr.get_collection(child_id).await.unwrap().is_none(),
        "child created during the delete is still live under a deleted parent"
    );
}

/// A create that waits on a parent being deleted is refused.
async fn create_waits_and_sees_the_deleted_parent(pool: &Pool) {
    if pool.dialect().name() == "sqlite" {
        return; // One writer at a time; `dead_parent_is_refused` covers it.
    }
    let mgr = manager(pool);
    let parent = create(&mgr, "wait-parent", None).await;

    // What `delete_collection` does: lock the row, soft-delete, commit.
    let mut tx = transaction_pool(pool).await.expect("begin");
    MediaCollection::objects()
        .where_(MediaCollection::id.eq(parent))
        .select_for_update()
        .fetch_tx(&mut tx)
        .await
        .expect("lock parent");
    let creating = tokio::spawn({
        let mgr = mgr.clone();
        async move {
            mgr.create_collection("c", "wait-child", Some(parent), "")
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !creating.is_finished(),
        "the create did not wait for the parent"
    );
    let soft_delete = MediaCollection::objects()
        .where_(MediaCollection::id.eq(parent))
        .update()
        .set("deleted_at", Utc::now())
        .compile()
        .expect("compile");
    update_tx(&mut tx, &soft_delete).await.expect("soft-delete");
    tx.commit().await.expect("commit");

    let r = creating.await.unwrap();
    assert!(is_not_found(&r), "created under a deleted parent: {r:?}");
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        dead_parent_is_refused,
        delete_takes_a_child_created_meanwhile,
        create_waits_and_sees_the_deleted_parent,
    ]
);
