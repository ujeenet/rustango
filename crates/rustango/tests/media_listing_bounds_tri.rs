//! Media listings return a page and tagging costs a fixed number of
//! queries, on every backend (#1570).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "media",
    feature = "testkit",
    feature = "admin"
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::core::{BulkInsertQuery, Model as _, SqlValue};
use rustango::media::router::{media_router_with, MediaAction, MediaAuthorizer, MediaDecision};
use rustango::media::{
    Media, MediaCollection, MediaError, MediaManager, MediaTag, MediaTagLink, SaveOpts,
};
use rustango::sql::{bulk_insert_pool, raw_execute_pool, Auto, Pool};
use rustango::storage::{InMemoryStorage, StorageRegistry};
use rustango::test_assertions::QueryCounter;
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::{by_dialect, tri_dialect_test};
use tower::ServiceExt as _;

async fn setup(pool: &Pool) {
    drop_table(pool, "rustango_media_tag_links").await;
    fresh_table::<MediaTag>(pool).await;
    fresh_table::<MediaTagLink>(pool).await;
    fresh_table::<Media>(pool).await;
    fresh_table::<MediaCollection>(pool).await;
}

fn manager(pool: &Pool) -> MediaManager {
    let registry = StorageRegistry::new()
        .set("default", Arc::new(InMemoryStorage::new()))
        .with_default("default");
    MediaManager::new_pool(pool.clone(), registry)
}

struct AllowAll;

#[async_trait::async_trait]
impl MediaAuthorizer for AllowAll {
    async fn authorize(&self, _: &axum::http::request::Parts, _: MediaAction) -> MediaDecision {
        MediaDecision::Allow
    }
}

/// `GET uri` through a permissive router: rows in the JSON array, queries run.
async fn get_rows(mgr: &MediaManager, uri: &str) -> (Vec<serde_json::Value>, usize) {
    let app = media_router_with(mgr.clone(), AllowAll);
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let (resp, queries) = QueryCounter::scope(async {
        let resp = app.oneshot(req).await.expect("router answers");
        (resp, QueryCounter::current())
    })
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    (rows, queries)
}

/// `n` rows of `(name, slug, parent_id, description)`, slugs `{prefix}{i:05}`.
async fn seed_collections(pool: &Pool, prefix: &str, n: usize, parent: Option<i64>) {
    let rows = (0..n)
        .map(|i| {
            let slug = format!("{prefix}{i:05}");
            vec![
                SqlValue::String(slug.clone()),
                SqlValue::String(slug),
                parent.map_or(SqlValue::Null, SqlValue::I64),
                SqlValue::String(String::new()),
            ]
        })
        .collect();
    let q = BulkInsertQuery::new(
        MediaCollection::SCHEMA,
        vec!["name", "slug", "parent_id", "description"],
        rows,
    );
    bulk_insert_pool(pool, &q).await.expect("seed collections");
}

async fn seed_media(mgr: &MediaManager, collection_id: Option<i64>) -> i64 {
    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "t/".into(),
            bytes: b"x".to_vec(),
            mime: "text/plain".into(),
            original_filename: "x.txt".into(),
            uploaded_by_id: None,
            collection_id,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("seed media");
    let Auto::Set(id) = m.id else { panic!("no id") };
    id
}

fn ids(rows: &[Media]) -> Vec<i64> {
    rows.iter()
        .map(|m| match m.id {
            Auto::Set(v) => v,
            Auto::Unset => panic!("no id"),
        })
        .collect()
}

/// `GET /collections` and `list_collections` return one page, in one query.
async fn collections_are_paged(pool: &Pool) {
    let mgr = manager(pool);
    seed_collections(pool, "c", 105, None).await;

    let (all, queries) = QueryCounter::scope(async {
        let all = mgr.list_collections().await.expect("list");
        (all, QueryCounter::current())
    })
    .await;
    assert_eq!((all.len(), queries), (100, 1), "rows, queries");
    let page = mgr.list_collections_paged(50, 100).await.expect("page");
    assert_eq!(page.len(), 5);
    let wide = mgr.list_collections_paged(5000, 0).await.expect("wide");
    assert_eq!(wide.len(), 105);

    let (rows, queries) = get_rows(&mgr, "/collections").await;
    assert_eq!((rows.len(), queries), (100, 1), "rows, queries");
    let (rows, _) = get_rows(&mgr, "/collections?limit=2&offset=1").await;
    let slugs: Vec<&str> = rows.iter().map(|r| r["slug"].as_str().unwrap()).collect();
    assert_eq!(slugs, ["c00001", "c00002"]);
}

/// `GET /tags` reads one page of tags by slug, in one query.
async fn tags_are_paged(pool: &Pool) {
    let mgr = manager(pool);
    let rows = (0..105)
        .map(|i| {
            let slug = format!("t{i:05}");
            vec![SqlValue::String(slug.clone()), SqlValue::String(slug)]
        })
        .collect();
    let q = BulkInsertQuery::new(MediaTag::SCHEMA, vec!["name", "slug"], rows);
    bulk_insert_pool(pool, &q).await.expect("seed tags");

    let (rows, queries) = get_rows(&mgr, "/tags").await;
    assert_eq!((rows.len(), queries), (100, 1), "rows, queries");
    let (rows, _) = get_rows(&mgr, "/tags?limit=2&offset=1").await;
    let slugs: Vec<&str> = rows.iter().map(|r| r["slug"].as_str().unwrap()).collect();
    assert_eq!(slugs, ["t00001", "t00002"]);
}

/// `set_tags` / `tag` cost a fixed number of queries, not one per slug.
async fn tagging_is_batched(pool: &Pool) {
    let mgr = manager(pool);
    let media = seed_media(&mgr, None).await;
    let first: Vec<String> = (0..50).map(|i| format!("s{i:02}")).collect();
    let first: Vec<&str> = first.iter().map(String::as_str).collect();

    // Find, insert the missing, find again; then delete + insert links.
    let queries = QueryCounter::scope(async {
        mgr.set_tags(media, &first).await.expect("set_tags");
        QueryCounter::current()
    })
    .await;
    assert_eq!(queries, 5, "set_tags with 50 new slugs");
    assert_eq!(mgr.tags_for(media).await.unwrap().len(), 50);

    // 25 known + 25 new; a repeat slug is one tag.
    let more: Vec<String> = (25..75).map(|i| format!("s{i:02}")).collect();
    let mut more: Vec<&str> = more.iter().map(String::as_str).collect();
    more.push("s30");
    let queries = QueryCounter::scope(async {
        mgr.tag(media, &more).await.expect("tag");
        QueryCounter::current()
    })
    .await;
    assert_eq!(queries, 4, "tag with 25 new slugs");
    assert_eq!(mgr.tags_for(media).await.unwrap().len(), 75);

    // Every tag known: one find, one link insert.
    let queries = QueryCounter::scope(async {
        mgr.tag(media, &more).await.expect("re-tag");
        QueryCounter::current()
    })
    .await;
    assert_eq!(queries, 2, "tag with no new slugs");
    assert_eq!(mgr.tags_for(media).await.unwrap().len(), 75);

    let too_many: Vec<String> = (0..1001).map(|i| format!("x{i}")).collect();
    let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
    let r = mgr.set_tags(media, &too_many).await;
    assert!(matches!(r, Err(MediaError::Other(_))), "{r:?}");
    assert_eq!(
        mgr.tags_for(media).await.unwrap().len(),
        75,
        "a refused set changes nothing"
    );
}

/// Two new slugs that MySQL's collation treats as one.
async fn case_variants_resolve(pool: &Pool) {
    let mgr = manager(pool);
    let media = seed_media(&mgr, None).await;
    mgr.set_tags(media, &["Mixed", "mixed"])
        .await
        .expect("set_tags");
    let expected = by_dialect! { pool,
        postgres => 2, because "slugs compare case-sensitively",
        mysql    => 1, because "the default collation is case-insensitive",
        sqlite   => 2, because "slugs compare case-sensitively",
    };
    assert_eq!(
        mgr.tags_for(media).await.unwrap().len(),
        expected.value,
        "{}",
        expected.why
    );
}

/// A subtree wider than the bind cap still lists one correct page.
async fn recursive_listing_spans_bind_cap(pool: &Pool) {
    let mgr = manager(pool);
    let root = mgr
        .create_collection("root", "root", None, "")
        .await
        .expect("root");
    let Auto::Set(root) = root.id else {
        panic!("no id")
    };
    let n = pool.dialect().max_bind_params();
    seed_collections(pool, "w", n, Some(root)).await;
    if pool.dialect().name() == "postgres" {
        // Unanalyzed, PG walks the tree with a nested loop: ~40s, not ~40ms.
        raw_execute_pool(pool, "ANALYZE rustango_media_collections", vec![])
            .await
            .expect("analyze");
    }
    let last = mgr
        .get_collection_by_slug(&format!("w{:05}", n - 1))
        .await
        .unwrap()
        .expect("last child");
    let Auto::Set(last) = last.id else {
        panic!("no id")
    };

    let older = seed_media(&mgr, Some(root)).await;
    let newer = seed_media(&mgr, Some(last)).await;

    // n + 1 ids: the descendant walk, then two `IN` lists.
    let (first, queries) = QueryCounter::scope(async {
        let rows = mgr
            .list_in_collection_paged(root, true, 1, 0)
            .await
            .expect("page 1");
        (rows, QueryCounter::current())
    })
    .await;
    assert_eq!((ids(&first), queries), (vec![newer], 3), "rows, queries");
    let second = mgr
        .list_in_collection_paged(root, true, 1, 1)
        .await
        .expect("page 2");
    assert_eq!(ids(&second), vec![older]);
    let both = mgr.list_in_collection(root, true).await.expect("all");
    assert_eq!(ids(&both), vec![newer, older]);
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        collections_are_paged,
        tags_are_paged,
        tagging_is_batched,
        case_variants_resolve,
        recursive_listing_spans_bind_cap,
    ]
);
