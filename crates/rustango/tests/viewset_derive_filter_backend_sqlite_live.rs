//! `#[viewset(filter_backend = …)]` narrows every action, as the builder's
//! `.filter_backend(…)` does.
#![cfg(all(feature = "sqlite", feature = "admin"))]

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, ModelSchema, Op, SqlValue, WhereExpr};
use rustango::sql::{Auto, Pool};
use rustango::viewset::ViewSetFilter;
use rustango::{Model, ViewSet};
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vsfb_post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 60)]
    pub status: String,
}

/// Only published posts.
pub struct Published;

impl ViewSetFilter for Published {
    fn filter(
        &self,
        _params: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        let f = schema.field("status").expect("status field");
        vec![WhereExpr::Predicate(Filter::new(
            f.column,
            Op::Eq,
            SqlValue::from("published"),
        ))]
    }
}

#[derive(ViewSet)]
#[viewset(model = Post, fields = "id, status", filter_backend = Published, read_only)]
pub struct PostViewSet;

async fn get(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn derive_filter_backend_scopes_list_and_retrieve() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE vsfb_post (id INTEGER PRIMARY KEY AUTOINCREMENT, status TEXT NOT NULL)",
        Vec::new(),
    )
    .await
    .unwrap();
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO vsfb_post (status) VALUES ('published'), ('draft')",
        Vec::new(),
    )
    .await
    .unwrap();
    let app = PostViewSet::router("/posts", pool);
    let (status, body) = get(app.clone(), "/posts").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["results"].as_array().expect("results");
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["status"], "published");
    let (status, _) = get(app, "/posts/2").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the draft is out of scope");
}
