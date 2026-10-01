//! A form body to a serializer ViewSet types array, hstore and bytes
//! fields the way JSON would, so they validate and save (#1993 review).
//!
//! Skips when `DATABASE_URL` is unset or `hstore` can't be created.

#![cfg(all(
    feature = "postgres",
    feature = "tenancy",
    feature = "serializer",
    feature = "testkit"
))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Array, Auto, HStore, Pool};
use rustango::{Model, Serializer};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_typed_form_post")]
#[rustango(app = "vs_typed_form")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub tags: Array<String>,
    pub attrs: HStore,
    pub blob: Vec<u8>,
}

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
struct PostSerializer {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    pub tags: Array<String>,
    pub attrs: HStore,
    pub blob: Vec<u8>,
}

async fn app() -> Option<axum::Router> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}"));
    sqlx::query("CREATE EXTENSION IF NOT EXISTS hstore")
        .execute(&pg)
        .await
        .ok()?;
    let pool = Pool::from(pg);
    rustango::testkit::matrix::fresh_table::<Post>(&pool).await;
    Some(
        rustango::viewset::ViewSet::for_model(Post::SCHEMA)
            .serializer::<PostSerializer>()
            .router_pool("/posts", pool),
    )
}

fn form(method: Method, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn form_array_hstore_and_bytes_fields_validate_and_save() {
    let Some(app) = app().await else {
        return;
    };
    // `attrs={"k":"v"}`, `blob=0aff` (hex).
    let resp = app
        .clone()
        .oneshot(form(
            Method::POST,
            "/posts",
            "tags=a%2Cb&attrs=%7B%22k%22%3A%22v%22%7D&blob=0aff",
        ))
        .await
        .unwrap();
    let status = resp.status();
    let v = json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["tags"], serde_json::json!(["a", "b"]), "{v}");
    assert_eq!(v["attrs"], serde_json::json!({"k": "v"}), "{v}");
    assert_eq!(v["blob"], serde_json::json!([10, 255]), "{v}");

    let id = v["id"].as_i64().unwrap();
    for method in [Method::PUT, Method::PATCH] {
        let resp = app
            .clone()
            .oneshot(form(
                method.clone(),
                &format!("/posts/{id}"),
                "tags=c&attrs=%7B%7D&blob=01",
            ))
            .await
            .unwrap();
        let status = resp.status();
        let v = json(resp).await;
        assert_eq!(status, StatusCode::OK, "{method}: {v}");
        assert_eq!(v["tags"], serde_json::json!(["c"]), "{method}: {v}");
    }
}
