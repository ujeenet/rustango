//! End-to-end live test for `ViewSet` per-action throttling on SQLite
//! (DRF `throttle_classes` parity, #1010). Fixed-window, process-local,
//! keyed by client (ConnectInfo → X-Forwarded-For → global). Asserts the
//! limit trips with a 429 + `Retry-After`, buckets are per-client, and
//! throttles are per-action.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::viewset::{ViewSet, ViewSetThrottle};
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_thr_post")]
#[rustango(app = "vs_thr_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn router(throttle: ViewSetThrottle) -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_thr_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    ViewSet::for_model(Post::SCHEMA)
        .throttle(throttle)
        .router_pool("/posts", Pool::Sqlite(sq))
}

/// A GET with an optional `X-Forwarded-For` client identity.
fn get(xff: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(Method::GET).uri("/posts");
    if let Some(ip) = xff {
        b = b.header("x-forwarded-for", ip);
    }
    b.body(Body::empty()).unwrap()
}

async fn status(app: &axum::Router, req: Request<Body>) -> StatusCode {
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn throttle_trips_with_429_and_retry_after() {
    // 2 list requests per 60s window.
    let app = router(ViewSetThrottle::all(2, 60)).await;

    assert_eq!(status(&app, get(Some("9.9.9.9"))).await, StatusCode::OK);
    assert_eq!(status(&app, get(Some("9.9.9.9"))).await, StatusCode::OK);

    // Third within the window is throttled, with a Retry-After header.
    let resp = app.clone().oneshot(get(Some("9.9.9.9"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        resp.headers().get(header::RETRY_AFTER).is_some(),
        "429 should carry Retry-After"
    );
}

#[tokio::test]
async fn throttle_buckets_are_per_client() {
    let app = router(ViewSetThrottle::all(2, 60)).await;

    // Exhaust client A's window.
    assert_eq!(status(&app, get(Some("1.1.1.1"))).await, StatusCode::OK);
    assert_eq!(status(&app, get(Some("1.1.1.1"))).await, StatusCode::OK);
    assert_eq!(
        status(&app, get(Some("1.1.1.1"))).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // A different client has its own fresh bucket.
    assert_eq!(status(&app, get(Some("2.2.2.2"))).await, StatusCode::OK);
}

#[tokio::test]
async fn throttle_is_per_action() {
    // Throttle only `list`; `create` (POST) stays unlimited.
    let throttle = ViewSetThrottle {
        list: Some(rustango::viewset::ThrottleRule::new(1, 60)),
        ..ViewSetThrottle::default()
    };
    let app = router(throttle).await;

    // First list OK, second throttled.
    assert_eq!(status(&app, get(Some("3.3.3.3"))).await, StatusCode::OK);
    assert_eq!(
        status(&app, get(Some("3.3.3.3"))).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // create is a different action — not throttled.
    let create = Request::builder()
        .method(Method::POST)
        .uri("/posts")
        .header("x-forwarded-for", "3.3.3.3")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"title":"hi"}"#))
        .unwrap();
    assert!(
        app.clone()
            .oneshot(create)
            .await
            .unwrap()
            .status()
            .is_success(),
        "create must not be throttled by the list rule"
    );
}
