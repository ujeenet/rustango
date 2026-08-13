//! ViewSet answers 401 for anonymous, 403 for authenticated-but-unauthorised
//! (#1193).
//!
//! The distinction is not cosmetic. A token client treats **401** as its cue
//! to refresh; answering **403** to a request that simply carried no
//! credentials means the refresh never fires and the member is silently
//! logged out. 403 must mean "you are known, and still may not do this".

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::viewset::{ViewSet, ViewSetPerms};
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_401_note")]
#[rustango(app = "vs_401_app")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query(
        "CREATE TABLE vs_401_note (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .unwrap();
    Pool::Sqlite(sq)
}

/// A ViewSet gated on a codename nobody holds.
fn gated(pool: Pool) -> axum::Router {
    ViewSet::for_model(Note::SCHEMA)
        .permissions(ViewSetPerms {
            list: vec!["vs_401_app.view_note".to_owned()],
            ..ViewSetPerms::default()
        })
        .router_pool("/notes", pool)
}

/// No credentials at all → 401, so a token client knows to refresh.
#[tokio::test]
async fn anonymous_request_gets_401() {
    let app = gated(pool().await);

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/notes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "an unauthenticated request must be 401, not 403"
    );
}

/// Authenticated, but without the codename → 403. This is the case 403 is for,
/// and it must NOT regress into 401 (which would send clients into a refresh
/// loop over a permission they will never be granted).
#[tokio::test]
async fn authenticated_but_unauthorised_gets_403() {
    let app = gated(pool().await);

    let mut req = Request::builder()
        .method(Method::GET)
        .uri("/notes")
        .body(Body::empty())
        .unwrap();
    // Present a principal the way the auth middleware does.
    req.extensions_mut()
        .insert(rustango::tenancy::middleware::AuthenticatedUser {
            id: 42,
            username: "member".to_owned(),
            is_superuser: false,
        });

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "a known principal lacking the codename must stay 403"
    );
}

/// A superuser passes, so the split didn't break the allow path.
#[tokio::test]
async fn superuser_is_allowed() {
    let app = gated(pool().await);

    let mut req = Request::builder()
        .method(Method::GET)
        .uri("/notes")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(rustango::tenancy::middleware::AuthenticatedUser {
            id: 1,
            username: "root".to_owned(),
            is_superuser: true,
        });

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// An ungated ViewSet is unaffected — no codenames means no auth check.
#[tokio::test]
async fn ungated_viewset_still_serves_anonymous() {
    let app = ViewSet::for_model(Note::SCHEMA).router_pool("/notes", pool().await);

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/notes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
