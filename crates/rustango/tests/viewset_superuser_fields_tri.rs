//! A ViewSet keeps the admin's superuser-only fields locked, on every backend.

#![cfg(all(feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::core::Column as _;
use rustango::core::Model as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::middleware::AuthenticatedUser;
use rustango::tenancy::User;
use rustango::tri_dialect_test;
use rustango::viewset::ViewSet;
use tower::ServiceExt as _;

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

/// `/users` over `rustango_users`, the caller signed in as `superuser` or not.
fn app(pool: &Pool, superuser: bool) -> axum::Router {
    ViewSet::for_model(User::SCHEMA)
        .allow_anonymous()
        .router_pool("/users", pool.clone())
        .layer(axum::Extension(AuthenticatedUser {
            id: 1,
            username: "caller".into(),
            is_superuser: superuser,
        }))
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let res = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn by_name(pool: &Pool, username: &str) -> User {
    User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch(pool)
        .await
        .expect("fetch")
        .pop()
        .expect("user")
}

async fn seed(pool: &Pool) -> (i64, String) {
    let mut u = User {
        username: unique("vsu"),
        password_hash: "old-hash".into(),
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed");
    (*u.id.get().expect("pk"), u.username)
}

async fn a_member_cannot_grant_superuser_or_set_a_hash(pool: &Pool) {
    let (id, _) = seed(pool).await;
    let renamed = unique("vsr");
    let body = serde_json::json!({
        "username": renamed, "is_superuser": true, "password_hash": "new-hash"
    });
    let (status, res) = send(app(pool, false), "PATCH", &format!("/users/{id}"), body).await;
    assert!(status.is_success(), "{status}: {res}");
    let u = by_name(pool, &renamed).await;
    assert!(!u.is_superuser, "is_superuser stays locked");
    assert_eq!(u.password_hash, "old-hash");
}

async fn a_member_cannot_create_a_superuser(pool: &Pool) {
    let name = unique("vsc");
    let body = serde_json::json!({
        "username": name, "password_hash": "h", "is_superuser": true,
        "active": true, "created_at": "2024-01-01T00:00:00Z", "data": {}
    });
    let (status, res) = send(app(pool, false), "POST", "/users", body).await;
    assert_eq!(status, StatusCode::CREATED, "{res}");
    assert!(!by_name(pool, &name).await.is_superuser);
}

/// The control: a superuser principal writes them.
async fn a_superuser_writes_them(pool: &Pool) {
    let (id, name) = seed(pool).await;
    let body = serde_json::json!({ "is_superuser": true, "password_hash": "new-hash" });
    let (status, res) = send(app(pool, true), "PATCH", &format!("/users/{id}"), body).await;
    assert!(status.is_success(), "{status}: {res}");
    let u = by_name(pool, &name).await;
    assert!(u.is_superuser);
    assert_eq!(u.password_hash, "new-hash");
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        a_member_cannot_grant_superuser_or_set_a_hash,
        a_member_cannot_create_a_superuser,
        a_superuser_writes_them,
    ],
);
