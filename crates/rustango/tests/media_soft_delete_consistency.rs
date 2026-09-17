#![cfg(all(feature = "sqlite", feature = "media", feature = "testkit"))]
//! The media surface must not contradict itself about what is deleted.
//!
//! Three separate reads of "is this row gone?" disagreed:
//!
//! - `popular_tags` counted links to soft-deleted media, while
//!   `list_with_tag` filtered them out — so `GET /tags/popular` and
//!   `GET /tags/{slug}/media` reported different worlds.
//! - `purge` hard-deleted the media row and left its tag links behind.
//!   Nothing ever reclaimed them, so the nightly sweep *manufactured*
//!   the inflation above.
//! - `delete_collection` soft-deleted one collection and left its
//!   children pointing at a parent that no longer resolves, which makes
//!   `collection_path` on the whole subtree a permanent error.

use std::sync::Arc;

use rustango::media::{MediaManager, SaveOpts};
use rustango::sql::{Auto, Pool};
use rustango::storage::{InMemoryStorage, StorageRegistry};

async fn manager() -> MediaManager {
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

async fn seed(mgr: &MediaManager, collection_id: Option<i64>) -> rustango::media::Media {
    mgr.save_bytes(SaveOpts {
        disk: "default".into(),
        key_prefix: "t/".into(),
        bytes: b"x".to_vec(),
        mime: "text/plain".into(),
        original_filename: "x.txt".into(),
        uploaded_by_id: Some(1),
        collection_id,
        metadata: serde_json::json!({}),
    })
    .await
    .expect("seed")
}

fn id_of(m: &rustango::media::Media) -> i64 {
    match m.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    }
}

async fn use_count(mgr: &MediaManager, slug: &str) -> i64 {
    mgr.popular_tags(100)
        .await
        .expect("popular_tags")
        .into_iter()
        .find(|(t, _)| t.slug == slug)
        .map(|(_, n)| n)
        .unwrap_or(0)
}

/// `popular_tags` and `list_with_tag` must agree about what exists.
#[tokio::test]
async fn a_soft_deleted_row_is_not_counted_as_a_tag_use() {
    let mgr = manager().await;
    let a = seed(&mgr, None).await;
    let b = seed(&mgr, None).await;
    mgr.tag(id_of(&a), &["featured"]).await.expect("tag a");
    mgr.tag(id_of(&b), &["featured"]).await.expect("tag b");

    assert_eq!(
        use_count(&mgr, "featured").await,
        2,
        "control: both counted"
    );

    mgr.delete(&a).await.expect("soft delete a");
    mgr.delete(&b).await.expect("soft delete b");

    let listed = mgr
        .list_with_tag("featured", 100, 0)
        .await
        .expect("list_with_tag")
        .len();
    let counted = use_count(&mgr, "featured").await;

    assert_eq!(listed, 0, "control: list_with_tag excludes soft-deleted");
    assert_eq!(
        counted, 0,
        "popular_tags reported {counted} uses of `featured` while list_with_tag \
         returned {listed} rows. Both are API-reachable (GET /tags/popular and \
         GET /tags/{{slug}}/media), so the surface contradicts itself — and the \
         count of soft-deleted rows leaks through a listing grant."
    );
}

/// A hard delete must not leave tag links behind.
#[tokio::test]
async fn purging_a_row_reclaims_its_tag_links() {
    let mgr = manager().await;
    let m = seed(&mgr, None).await;
    let id = id_of(&m);
    mgr.tag(id, &["archived"]).await.expect("tag");

    assert_eq!(
        use_count(&mgr, "archived").await,
        1,
        "control: counted once"
    );

    mgr.purge(&m).await.expect("purge");

    assert!(
        mgr.tags_for(id).await.expect("tags_for").is_empty(),
        "the media row is gone but its tag links survive — `media_id` carries no \
         foreign key, so nothing reclaims them"
    );
    let after = use_count(&mgr, "archived").await;
    assert_eq!(
        after, 0,
        "`archived` still reports {after} uses after the only row carrying it was \
         purged — a permanent phantom, manufactured by the nightly sweep itself"
    );
}

/// Deleting a collection must not leave its children dangling.
#[tokio::test]
async fn deleting_a_collection_takes_its_subtree_with_it() {
    let mgr = manager().await;
    let parent = mgr
        .create_collection("Parent", "parent", None, "")
        .await
        .expect("parent");
    let pid = match parent.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    let child = mgr
        .create_collection("Child", "child", Some(pid), "")
        .await
        .expect("child");
    let cid = match child.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    let inside = seed(&mgr, Some(cid)).await;

    mgr.delete_collection(pid).await.expect("delete parent");

    let visible: Vec<String> = mgr
        .list_collections()
        .await
        .expect("list")
        .into_iter()
        .map(|c| c.slug)
        .collect();
    assert!(
        !visible.contains(&"child".to_owned()),
        "the child collection is still listed after its parent was deleted, pointing \
         at a parent that no longer resolves — a tree renderer gets a dangling edge, \
         and `collection_path` on the subtree is a permanent error. Listed: {visible:?}"
    );

    // The documented contract: media inside is orphaned, not deleted.
    // That must hold for the whole subtree, not just the top level.
    let still_there = mgr.get(id_of(&inside)).await.expect("get");
    assert!(
        still_there.is_some(),
        "media inside a descendant collection was deleted — the contract is that it \
         is orphaned, not destroyed"
    );
    assert_eq!(
        still_there.and_then(|m| m.collection_id),
        None,
        "media inside a descendant collection kept a collection_id pointing at a \
         deleted collection"
    );
}
