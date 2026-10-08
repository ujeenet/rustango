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
use rustango::core::{BulkInsertQuery, Model as _, SqlValue};
use rustango::media::{Media, MediaCollection, MediaError, MediaManager};
use rustango::sql::{
    bulk_insert_pool, sqlx, transaction_pool, update_tx, Auto, FetcherTx as _, Pool,
};
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

/// The router answers 404 for "not found", so a bad `parent_id` avoids it (400).
fn is_dead_parent(r: &Result<MediaCollection, MediaError>) -> bool {
    matches!(r, Err(MediaError::Other(m))
        if m.contains("missing or deleted") && !m.contains("not found"))
}

/// MySQL 1213 / PG 40P01: the server rolled one side back.
fn is_deadlock(e: &MediaError) -> bool {
    matches!(e, MediaError::Db(sqlx::Error::Database(d))
        if matches!(d.code().as_deref(), Some("40001" | "40P01")))
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
    assert!(is_dead_parent(&r), "{r:?}");
    let r = mgr
        .create_collection("y", "seq-y", Some(i64::MAX), "")
        .await;
    assert!(is_dead_parent(&r), "{r:?}");
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
    assert!(is_dead_parent(&r), "created under a deleted parent: {r:?}");
}

/// SQLite: a writer that holds the lock makes both wait, not fail with
/// `SQLITE_BUSY` after reading first (#2180).
async fn writers_wait_their_turn(pool: &Pool) {
    let mgr = manager(pool);
    let root = create(&mgr, "busy-root", None).await;
    let mid = create(&mgr, "busy-mid", Some(root)).await;
    let other = create(&mgr, "busy-other", None).await;

    let mut tx = transaction_pool(pool).await.expect("begin");
    let mut hold = MediaCollection {
        id: Auto::Unset,
        name: "busy-hold".into(),
        slug: "busy-hold".into(),
        parent_id: None,
        description: String::new(),
        created_at: Auto::Unset,
        deleted_at: None,
    };
    hold.insert_tx(&mut tx).await.expect("take the write lock");
    let deleting = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.delete_collection(root).await }
    });
    let creating = tokio::spawn({
        let mgr = mgr.clone();
        async move {
            mgr.create_collection("n", "busy-new", Some(other), "")
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    tx.commit().await.expect("commit");

    deleting
        .await
        .unwrap()
        .expect("delete waits for the writer");
    creating
        .await
        .unwrap()
        .expect("create waits for the writer");
    assert!(mgr.get_collection(mid).await.unwrap().is_none());
}

/// A level wider than one `IN` list's bind limit is walked in chunks.
async fn wide_subtree_is_chunked(pool: &Pool) {
    let mgr = manager(pool);
    let root = create(&mgr, "wide-root", None).await;
    let n = pool.dialect().max_bind_params() + 1;
    let rows = (0..n)
        .map(|i| {
            vec![
                SqlValue::String(format!("w{i}")),
                SqlValue::String(format!("w{i}")),
                SqlValue::I64(root),
                SqlValue::String(String::new()),
            ]
        })
        .collect();
    let q = BulkInsertQuery::new(
        MediaCollection::SCHEMA,
        vec!["name", "slug", "parent_id", "description"],
        rows,
    );
    bulk_insert_pool(pool, &q).await.expect("seed a wide level");
    mgr.delete_collection(root).await.expect("chunked delete");
    let live = mgr.list_collections_paged(1000, 0).await.expect("list");
    assert!(live.is_empty(), "{} live after the delete", live.len());
}

/// #2182 — parallel creates, some over a tombstone, never deadlock: MySQL
/// gap-locked the slug index on the tombstone DELETE.
async fn parallel_creates_do_not_deadlock(pool: &Pool) {
    let mgr = manager(pool);
    for round in 0..10 {
        for i in 0..2 {
            let id = create(&mgr, &format!("pc{round}-{i}"), None).await;
            mgr.delete_collection(id).await.expect("delete");
        }
        let creates: Vec<_> = (0..5)
            .map(|i| {
                let mgr = mgr.clone();
                let slug = format!("pc{round}-{i}");
                tokio::spawn(async move { mgr.create_collection(&slug, &slug, None, "").await })
            })
            .collect();
        for c in creates {
            c.await.unwrap().expect("parallel create");
        }
    }
}

/// A delete and a create at once, at every depth: no live child is left
/// under a deleted parent, and a deadlock is a clear error, never a hang.
async fn concurrent_writes_stay_consistent(pool: &Pool) {
    let mgr = manager(pool);
    let mut deadlocks = 0;
    for round in 0..20 {
        let root = create(&mgr, &format!("st{round}-root"), None).await;
        let a = create(&mgr, &format!("st{round}-a"), Some(root)).await;
        let b = create(&mgr, &format!("st{round}-b"), Some(root)).await;
        let c = create(&mgr, &format!("st{round}-c"), Some(a)).await;
        let d = create(&mgr, &format!("st{round}-d"), Some(b)).await;
        let parent = [root, a, b, c, d][round % 5];
        let creating = tokio::spawn({
            let mgr = mgr.clone();
            let slug = format!("st{round}-new");
            async move { mgr.create_collection(&slug, &slug, Some(parent), "").await }
        });
        let deleting = tokio::spawn({
            let mgr = mgr.clone();
            async move { mgr.delete_collection(root).await }
        });
        match tokio::time::timeout(Duration::from_secs(30), creating)
            .await
            .expect("create hung")
            .unwrap()
        {
            Ok(_) => {}
            Err(e) if is_deadlock(&e) => deadlocks += 1,
            r @ Err(_) => assert!(is_dead_parent(&r), "{r:?}"),
        }
        match tokio::time::timeout(Duration::from_secs(30), deleting)
            .await
            .expect("delete hung")
            .unwrap()
        {
            Ok(()) => {}
            Err(e) if is_deadlock(&e) => {
                deadlocks += 1;
                mgr.delete_collection(root).await.expect("retry delete");
            }
            Err(e) => panic!("delete: {e}"),
        }
        let live = mgr.list_collections_paged(1000, 0).await.expect("list");
        let ids: std::collections::HashSet<_> = live
            .iter()
            .filter_map(|c| match c.id {
                Auto::Set(id) => Some(id),
                Auto::Unset => None,
            })
            .collect();
        for c in &live {
            if let Some(p) = c.parent_id {
                assert!(ids.contains(&p), "{} is live under deleted {p}", c.slug);
            }
        }
    }
    eprintln!(
        "{}: {deadlocks} deadlock(s) in 20 rounds",
        pool.dialect().name()
    );
}

tri_dialect_test!(
    setup: setup,
    sqlite: file,
    scenarios: [
        writers_wait_their_turn,
        wide_subtree_is_chunked,
        concurrent_writes_stay_consistent,
        parallel_creates_do_not_deadlock,
        dead_parent_is_refused,
        delete_takes_a_child_created_meanwhile,
        create_waits_and_sees_the_deleted_parent,
    ]
);
