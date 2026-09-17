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
    manager_with_pool().await.0
}

/// Same, but hands back the pool so a test can reach the database
/// directly — needed to install a trigger that fails one statement.
async fn manager_with_pool() -> (MediaManager, Pool) {
    // `min_connections(2)` is load-bearing, not arbitrary: it opens both
    // connections eagerly, so a seed on one and a read on the other
    // would fail loudly if `sqlite::memory:` did not share a single
    // database across the pool. Every test here seeds then reads back,
    // which is the control. Dropping it to 1 would make the suite pass
    // for a reason it does not intend.
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
    (
        MediaManager::new_pool(pool_enum.clone(), registry),
        pool_enum,
    )
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

    // The collection the caller *named*, asserted first and separately.
    //
    // The original version of this test checked only that the child was
    // gone and the media orphaned — so when a bind-order bug put the
    // first id into `deleted_at` and the timestamp into the `IN` list,
    // the named collection silently survived on SQLite and MySQL and
    // this suite stayed green. `collect_descendant_ids` returns the root
    // first, so the named row is precisely the one that broke.
    assert!(
        mgr.get_collection(pid).await.expect("get parent").is_none(),
        "the collection that was named is still live after delete_collection —          the one row the caller explicitly asked to delete"
    );

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

/// A collection with no children is the case that broke.
///
/// With one id in the list, a bind-order error is total: the single id
/// goes into `deleted_at` and the timestamp into `IN (…)`, so the
/// `UPDATE` matches nothing. On SQLite that is a silent no-op — the row
/// stays live while its media has already been orphaned, which is worse
/// than either outcome alone. On MySQL it is `ERROR 1292, Incorrect
/// datetime value: '1' for column 'deleted_at'`.
#[tokio::test]
async fn deleting_a_childless_collection_deletes_it() {
    let mgr = manager().await;
    let c = mgr
        .create_collection("Leaf", "leaf", None, "")
        .await
        .expect("leaf");
    let cid = match c.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    let inside = seed(&mgr, Some(cid)).await;

    mgr.delete_collection(cid).await.expect("delete leaf");

    assert!(
        mgr.get_collection(cid).await.expect("get").is_none(),
        "a childless collection was not deleted"
    );
    let live: Vec<String> = mgr
        .list_collections()
        .await
        .expect("list")
        .into_iter()
        .map(|c| c.slug)
        .collect();
    assert!(
        !live.contains(&"leaf".to_owned()),
        "a childless collection is still listed after deletion: {live:?}"
    );
    // And the media it held was still orphaned, so the two halves agree.
    assert_eq!(
        mgr.get(id_of(&inside))
            .await
            .expect("get media")
            .and_then(|m| m.collection_id),
        None,
        "media was not orphaned"
    );
}

/// Deep tree: every level goes, and the root is not special-cased away.
#[tokio::test]
async fn a_deep_subtree_is_deleted_at_every_level() {
    let mgr = manager().await;
    let mut parent: Option<i64> = None;
    let mut ids = Vec::new();
    for i in 0..6 {
        let c = mgr
            .create_collection(format!("L{i}"), format!("l{i}"), parent, "")
            .await
            .expect("level");
        let id = match c.id {
            Auto::Set(v) => v,
            _ => panic!("no id"),
        };
        ids.push(id);
        parent = Some(id);
    }
    let deepest = seed(&mgr, Some(*ids.last().unwrap())).await;

    mgr.delete_collection(ids[0]).await.expect("delete root");

    let live: Vec<String> = mgr
        .list_collections()
        .await
        .expect("list")
        .into_iter()
        .map(|c| c.slug)
        .collect();
    assert!(
        live.is_empty(),
        "levels survived a subtree delete: {live:?} — the root is the CTE anchor and \
         is the level most likely to be mishandled"
    );
    assert!(
        mgr.get(id_of(&deepest)).await.expect("get").is_some(),
        "media five levels down was destroyed; the contract is that it is orphaned"
    );
}

// `purge_pending` is fixed the same way as `purge` — links first — but
// has **no test here, deliberately**. Creating a Pending row needs
// `begin_upload`, which calls `presigned_put_url`; `InMemoryStorage`
// does not implement it, so the call fails before inserting and the
// row cannot exist in this suite. A test would skip, and a skipping
// test that prints `ok` is what this whole PR stack exists to stop.
//
// Covered instead by: the statement shape executed against a live
// MySQL 8 and SQLite, and by symmetry with `purge` above, which is
// tested. That `InMemoryStorage` cannot reach the upload-ticket path at
// all is a coverage hole of its own, recorded on #1548.

/// A failed collection delete must not leave the media orphaned.
///
/// `delete_collection` is orphan-then-soft-delete. Run as two separate
/// statements, a failure of the second left the first committed: the
/// collection still live, every media row under it orphaned, and the
/// previous `collection_id` recorded nowhere. No undo, no log line, and
/// nothing in the error saying the orphaning had already happened — so
/// a retry "succeeds" with the association permanently gone. Any
/// mid-request blip does it: a failover, a lock timeout, a reset
/// connection.
///
/// The trigger fires on `UPDATE OF deleted_at` specifically. A cruder
/// break — renaming the collections table — makes the call fail at
/// `collect_descendant_ids`, which reads that same table, so the
/// orphaning never runs and the test passes while proving nothing.
#[tokio::test]
async fn a_failed_collection_delete_orphans_nothing() {
    let (mgr, pool) = manager_with_pool().await;
    let c = mgr
        .create_collection("Doomed", "doomed", None, "")
        .await
        .expect("collection");
    let cid = match c.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    };
    let inside = seed(&mgr, Some(cid)).await;

    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TRIGGER fail_soft_delete \
         BEFORE UPDATE OF deleted_at ON rustango_media_collections \
         BEGIN SELECT RAISE(ABORT, 'simulated failure'); END",
        Vec::new(),
    )
    .await
    .expect("install trigger");

    let err = mgr.delete_collection(cid).await;
    assert!(
        err.is_err(),
        "control: the delete must fail, or this test proves nothing"
    );

    // The orphaning is the first statement. Without a transaction it has
    // already committed by the time the second one fails.
    let after = mgr
        .get(id_of(&inside))
        .await
        .expect("get media")
        .expect("media row still exists");
    assert_eq!(
        after.collection_id,
        Some(cid),
        "the delete failed but the media was already orphaned — its previous \
         collection_id is recorded nowhere, so this is unrecoverable and a retry \
         cannot restore it"
    );
    assert!(
        mgr.get_collection(cid).await.expect("get").is_some(),
        "control: the collection should still be live after a failed delete"
    );
}
