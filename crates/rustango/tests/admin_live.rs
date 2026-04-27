//! Integration test for `rustango_admin::router`.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently. We boot
//! the router via `tower::ServiceExt::oneshot` (no socket required) and
//! make HTTP requests against it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::migrate;
use rustango::sql::sqlx;
use rustango::Model;
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "admin_user")]
pub struct AdminUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    name: String,
    #[rustango(min = 0, max = 150)]
    age: i32,
    is_active: bool,
}

fn live_lock() -> &'static Mutex<()> {
    static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn seed(pool: &sqlx::PgPool) {
    migrate::drop_all(pool).await.unwrap();
    migrate::apply_all(pool).await.unwrap();
    for user in [
        AdminUser {
            id: 1,
            name: "alice".into(),
            age: 30,
            is_active: true,
        },
        AdminUser {
            id: 2,
            name: "bob".into(),
            age: 45,
            is_active: false,
        },
    ] {
        user.insert(pool).await.unwrap();
    }
}

#[tokio::test]
async fn index_lists_registered_models() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("rustango admin"), "missing title: {body}");
    assert!(body.contains("AdminUser"), "missing model name: {body}");
    assert!(
        body.contains("href=\"/admin_user\""),
        "missing link to table: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn table_view_renders_seeded_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("AdminUser"), "missing heading: {body}");
    assert!(body.contains("2 rows"), "missing row count: {body}");
    assert!(body.contains("alice"), "missing alice: {body}");
    assert!(body.contains("bob"), "missing bob: {body}");
    // Numeric & boolean rendering
    assert!(body.contains(">30<"), "missing alice age 30: {body}");
    assert!(body.contains(">45<"), "missing bob age 45: {body}");
    assert!(body.contains(">true<"), "missing true: {body}");
    assert!(body.contains(">false<"), "missing false: {body}");
    // PK marker
    assert!(body.contains("(pk)"), "missing pk marker: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn table_view_empty_table_says_no_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("No rows"), "missing empty marker: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn table_view_unknown_table_returns_404() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    let app = rustango::admin::router(pool);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/no_such_table")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_string(response).await;
    assert!(
        body.contains("table not found"),
        "missing error message: {body}",
    );
}

#[tokio::test]
async fn html_escapes_user_provided_strings() {
    // A row whose name contains HTML special chars should not be rendered raw.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    AdminUser {
        id: 1,
        name: "<script>".into(),
        age: 30,
        is_active: true,
    }
    .insert(&pool)
    .await
    .unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = body_string(response).await;
    assert!(
        body.contains("&lt;script&gt;"),
        "raw script tag leaked: {body}",
    );
    assert!(
        !body.contains(">script<"),
        "unescaped script element: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}
