//! `ViewSetFilter::filter_with(&Parts, …)` — ownership scoping that holds on
//! every action, not just `list`.
//!
//! The principal lives in the request extensions, so "only this user's rows"
//! cannot be expressed against the params-only `filter`. And scoping `list`
//! alone is worse than not scoping at all: it reads as safe while every row
//! stays reachable by id. These tests pin both halves — the collection is
//! narrowed, and `retrieve` / `update` / `destroy` of a row belonging to
//! someone else is a **404**, because a 403 would confirm the id exists.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use std::collections::HashMap;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::{Filter, Model as _, ModelSchema, Op, SqlValue, WhereExpr};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::viewset::{ViewSet, ViewSetFilter};
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_pf_note")]
#[rustango(app = "vs_pf_app")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub owner_id: i64,
    #[rustango(max_length = 200)]
    pub body: String,
}

/// Stands in for an authenticated principal, injected as an extension the way
/// an auth middleware would.
#[derive(Clone, Copy)]
struct Principal(i64);

/// The backend an app writes: rows whose `owner_id` is the caller.
struct OwnerFilter;

impl ViewSetFilter for OwnerFilter {
    fn filter(&self, _p: &HashMap<String, String>, schema: &'static ModelSchema) -> Vec<WhereExpr> {
        // No principal in hand ⇒ match nothing. Fail closed: a backend that
        // silently returns "no predicates" would widen the query to everyone.
        deny(schema)
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _p: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        let Some(Principal(uid)) = parts.extensions.get::<Principal>().copied() else {
            return deny(schema);
        };
        schema.field("owner_id").map_or_else(Vec::new, |f| {
            vec![WhereExpr::Predicate(Filter {
                column: f.column,
                op: Op::Eq,
                value: SqlValue::from(uid),
            })]
        })
    }
}

fn deny(schema: &'static ModelSchema) -> Vec<WhereExpr> {
    schema.field("owner_id").map_or_else(Vec::new, |f| {
        vec![WhereExpr::Predicate(Filter {
            column: f.column,
            op: Op::Eq,
            value: SqlValue::from(-1_i64),
        })]
    })
}

async fn router() -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_pf_note (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            owner_id INTEGER NOT NULL, \
            body TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    for (owner, body) in [(1, "alice one"), (1, "alice two"), (2, "bob one")] {
        sqlx::query("INSERT INTO vs_pf_note (owner_id, body) VALUES (?, ?)")
            .bind(owner)
            .bind(body)
            .execute(&sq)
            .await
            .expect("seed");
    }
    ViewSet::for_model(Note::SCHEMA)
        .page_size(100)
        .filter_backend(OwnerFilter)
        .router_pool("/notes", Pool::Sqlite(sq))
}

/// A request carrying `uid` as the authenticated principal.
fn as_user(method: Method, uri: &str, uid: i64) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(Principal(uid));
    req
}

fn patch_as(uri: &str, uid: i64, form: &'static str) -> Request<Body> {
    let mut req = Request::builder()
        .method(Method::PATCH)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    req.extensions_mut().insert(Principal(uid));
    req
}

async fn json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn list_returns_only_the_principals_rows() {
    let app = router().await;
    let resp = app
        .clone()
        .oneshot(as_user(Method::GET, "/notes", 1))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json(resp).await;
    assert_eq!(body["count"], 2);
    for row in body["results"].as_array().expect("results") {
        assert_eq!(row["owner_id"], 1);
    }

    // The other member sees their own single row — not a filtered view of a
    // shared list, a different list.
    let resp = app
        .oneshot(as_user(Method::GET, "/notes", 2))
        .await
        .unwrap();
    let body = json(resp).await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["results"][0]["body"], "bob one");
}

#[tokio::test]
async fn retrieve_of_someone_elses_row_is_not_found() {
    let app = router().await;
    // Bob's row (id 3) is readable by Bob…
    let resp = app
        .clone()
        .oneshot(as_user(Method::GET, "/notes/3", 2))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // …and simply does not exist for Alice. 404, not 403: a 403 would tell
    // her the id is real.
    let resp = app
        .oneshot(as_user(Method::GET, "/notes/3", 1))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_cannot_reach_across_owners() {
    let app = router().await;
    let resp = app
        .clone()
        .oneshot(patch_as("/notes/3", 1, "body=owned+by+alice+now"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The row is untouched — the guard is the query, not the response code.
    let resp = app
        .clone()
        .oneshot(as_user(Method::GET, "/notes/3", 2))
        .await
        .unwrap();
    assert_eq!(json(resp).await["body"], "bob one");

    // The owner's own update still works, and reads back.
    let resp = app
        .clone()
        .oneshot(patch_as("/notes/3", 2, "body=edited"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json(resp).await["body"], "edited");
}

#[tokio::test]
async fn destroy_cannot_reach_across_owners() {
    let app = router().await;
    let resp = app
        .clone()
        .oneshot(as_user(Method::DELETE, "/notes/3", 1))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .clone()
        .oneshot(as_user(Method::GET, "/notes/3", 2))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "row survived the attempt");

    let resp = app
        .clone()
        .oneshot(as_user(Method::DELETE, "/notes/3", 2))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .oneshot(as_user(Method::GET, "/notes/3", 2))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_backend_without_a_principal_fails_closed() {
    // No extension on the request: the default `filter` runs and denies.
    let app = router().await;
    let req = Request::builder()
        .method(Method::GET)
        .uri("/notes")
        .body(Body::empty())
        .unwrap();
    let body = json(app.oneshot(req).await.unwrap()).await;
    assert_eq!(body["count"], 0);
}
