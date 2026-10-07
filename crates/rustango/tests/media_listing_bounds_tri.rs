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
use rustango::sql::{bulk_insert_pool, Auto, Pool};
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

tri_dialect_test!(
    setup: setup,
    scenarios: [
        collections_are_paged,
    ]
);
