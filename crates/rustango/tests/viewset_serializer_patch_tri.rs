//! A serializer ViewSet's PATCH validates the row the UPDATE will store
//! (#1995): only written fields overlay the stored row, inside the scope.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "tenancy",
    feature = "serializer"
))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::tenancy::Principal;
use rustango::viewset::{OwnedBy, ViewSet};
use rustango::{tri_dialect_test, Model, Serializer};
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_patch_span")]
#[rustango(app = "vs_patch_tri")]
pub struct Span {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub owner_id: i64,
    pub lo: i64,
    pub hi: i64,
    #[rustango(max_length = 50)]
    pub note: String,
}

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Span, validate = "ordered")]
struct SpanSerializer {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    #[serializer(validate = "positive")]
    pub owner_id: i64,
    pub lo: i64,
    pub hi: i64,
    #[serializer(validate = "long_note")]
    pub note: String,
}

impl SpanSerializer {
    fn ordered(&self) -> Result<(), rustango::forms::FormErrors> {
        let mut e = rustango::forms::FormErrors::default();
        if self.lo > self.hi {
            e.add_non_field(format!("lo {} > hi {}", self.lo, self.hi));
            return Err(e);
        }
        Ok(())
    }

    fn positive(n: &i64) -> Result<(), String> {
        if *n < 1 {
            return Err("owner must be positive".to_owned());
        }
        Ok(())
    }

    fn long_note(n: &String) -> Result<(), String> {
        if n.len() < 3 {
            return Err("note too short".to_owned());
        }
        Ok(())
    }
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Span>(pool).await;
}

fn plain(pool: &Pool) -> axum::Router {
    ViewSet::for_model(Span::SCHEMA)
        .serializer::<SpanSerializer>()
        .router_pool("/spans", pool.clone())
}

fn owned(pool: &Pool) -> axum::Router {
    ViewSet::for_model(Span::SCHEMA)
        .serializer::<SpanSerializer>()
        .filter_backend(OwnedBy::column("owner_id"))
        .router_pool("/spans", pool.clone())
}

async fn send(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: &str,
    uid: Option<i64>,
) -> (StatusCode, Value) {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    if let Some(uid) = uid {
        r.extensions_mut().insert(Principal::user(uid, false, None));
    }
    let resp = app.clone().oneshot(r).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Create `{lo, hi}` owned by `owner`; returns its id.
async fn seed(pool: &Pool, owner: i64, lo: i64, hi: i64) -> i64 {
    let body = format!(r#"{{"owner_id":{owner},"lo":{lo},"hi":{hi},"note":"seed"}}"#);
    let (status, v) = send(&plain(pool), Method::POST, "/spans", &body, None).await;
    assert_eq!(status, StatusCode::CREATED, "seed: {v}");
    v["id"].as_i64().expect("id")
}

async fn stored(pool: &Pool, id: i64) -> (i64, i64, i64) {
    let rows: Vec<Span> = Span::objects().fetch(pool).await.expect("fetch");
    let s = rows.iter().find(|s| s.id.get() == Some(&id)).expect("row");
    (s.owner_id, s.lo, s.hi)
}

/// `lo` is outside `fields()`, so the UPDATE never writes it: the check
/// must see the stored `lo`, not the sent one.
async fn patch_checks_the_stored_value_of_an_unwritten_field(pool: &Pool) {
    let id = seed(pool, 1, 1, 10).await;
    let app = ViewSet::for_model(Span::SCHEMA)
        .serializer::<SpanSerializer>()
        .fields(&["id", "hi"])
        .router_pool("/spans", pool.clone());
    let uri = format!("/spans/{id}");

    // Stored lo 1 > written hi 0: refused, though the sent lo 0 would pass.
    let (status, v) = send(&app, Method::PATCH, &uri, r#"{"lo":0,"hi":0}"#, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v.to_string().contains("lo 1 > hi 0"), "{v}");
    assert_eq!(stored(pool, id).await, (1, 1, 10));

    // Stored lo 1 <= hi 15: accepted, though the sent lo 20 would fail.
    let (status, v) = send(&app, Method::PATCH, &uri, r#"{"lo":20,"hi":15}"#, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(stored(pool, id).await, (1, 1, 15));
}

/// A pinned field the client sends is ignored, so its value cannot 422.
async fn patch_ignores_a_sent_pinned_field(pool: &Pool) {
    let id = seed(pool, 1, 1, 10).await;
    let uri = format!("/spans/{id}");
    let body = r#"{"owner_id":-5,"hi":11}"#;
    let (status, v) = send(&owned(pool), Method::PATCH, &uri, body, Some(1)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(stored(pool, id).await, (1, 1, 11));
}

/// Another owner's row is a 404, never a 422 carrying its stored values.
async fn patch_out_of_scope_row_is_404(pool: &Pool) {
    let id = seed(pool, 2, 1, 10).await;
    let uri = format!("/spans/{id}");
    let (status, v) = send(&owned(pool), Method::PATCH, &uri, r#"{"lo":20}"#, Some(1)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(stored(pool, id).await, (2, 1, 10));
}

/// No stored row: the body is not validated alone, the UPDATE answers 404.
async fn patch_missing_pk_is_404(pool: &Pool) {
    let (status, v) = send(
        &plain(pool),
        Method::PATCH,
        "/spans/999",
        r#"{"lo":20}"#,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
}

/// A wrong-typed field fails validation, naming the field.
async fn patch_wrong_type_is_a_field_error(pool: &Pool) {
    let id = seed(pool, 1, 1, 10).await;
    let uri = format!("/spans/{id}");
    let (status, v) = send(&plain(pool), Method::PATCH, &uri, r#"{"lo":"x"}"#, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["details"]["lo"].is_array(), "{v}");
}

/// PUT validates the whole body: an absent `note` fails its validator.
async fn put_validates_every_field(pool: &Pool) {
    let id = seed(pool, 1, 1, 10).await;
    let uri = format!("/spans/{id}");
    let body = r#"{"owner_id":1,"lo":1,"hi":10}"#;
    let (status, v) = send(&plain(pool), Method::PUT, &uri, body, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["details"]["note"].is_array(), "{v}");
}

/// #2010: the check read the row outside the UPDATE's transaction, so a
/// write committed in between went unseen.
async fn patch_checks_the_row_it_overwrites(pool: &Pool) {
    use rustango::core::{Assignment, Filter, Op, SqlValue, UpdateQuery, WhereExpr};
    let id = seed(pool, 1, 1, 10).await;
    // A concurrent writer holds the row: lo 1 -> 8, not committed yet.
    let mut tx = rustango::sql::transaction_pool(pool).await.expect("begin");
    let write = UpdateQuery::new(
        Span::SCHEMA,
        vec![Assignment::new("lo", SqlValue::I64(8))],
        WhereExpr::Predicate(Filter::new("id", Op::Eq, SqlValue::I64(id))),
    );
    rustango::sql::update_tx(&mut tx, &write)
        .await
        .expect("concurrent write");
    let app = plain(pool);
    let uri = format!("/spans/{id}");
    let patch =
        tokio::spawn(async move { send(&app, Method::PATCH, &uri, r#"{"hi":5}"#, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    tx.commit().await.expect("commit");
    // hi 5 passes over the old lo 1, not over the committed lo 8.
    let (status, v) = patch.await.expect("join");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(stored(pool, id).await, (1, 8, 10));
}

// A file database: in-memory SQLite blocks the stale read #2010 needs.
tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        patch_checks_the_row_it_overwrites,
        patch_checks_the_stored_value_of_an_unwritten_field,
        patch_ignores_a_sent_pinned_field,
        patch_out_of_scope_row_is_404,
        patch_missing_pk_is_404,
        patch_wrong_type_is_a_field_error,
        put_validates_every_field,
    ],
}
