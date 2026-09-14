//! `Cli::with_health()` must mount `/health` and `/ready` on every
//! backend, not only Postgres (#1457).
//!
//! It mounted them in the `#[cfg(feature = "postgres")]` arm of
//! `runserver` and nowhere else, so on a SQLite or MySQL build the
//! builder method set its flag, returned `self`, the server started —
//! and both endpoints answered 404. A load balancer or container
//! `HEALTHCHECK` aimed at `/health` then reported the service
//! permanently unhealthy, with nothing logged to say why.
//!
//! The pre-existing test asserted `with_health()` flips its own
//! boolean, which is true in every build and was true throughout the
//! bug. This one asks the only question that distinguishes them: does a
//! **request** get a 200?
//!
//! Revert the merge in the non-Postgres arm of `runserver` and this
//! fails under `--no-default-features --features sqlite,admin`.

#![cfg(all(feature = "admin", feature = "sqlite"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::sql::Pool;
use tower::ServiceExt;

/// Build the health router the way `runserver` does, from a `Pool`
/// enum rather than a driver-typed pool.
///
/// This is the seam the bug lived at: `health_router` takes
/// `impl Into<Pool>` and `crate::health` is gated on `admin`, not on a
/// backend — so nothing about a SQLite build prevented the mount. The
/// arm simply never performed it.
async fn health_app() -> axum::Router {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::health::health_router(pool)
}

#[tokio::test]
async fn health_answers_on_a_sqlite_pool() {
    let app = health_app().await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "/health must answer on a non-Postgres pool; it 404'd for every \
         SQLite and MySQL build before #1457"
    );
}

#[tokio::test]
async fn ready_answers_on_a_sqlite_pool() {
    let app = health_app().await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "/ready must answer on a non-Postgres pool (#1457)"
    );
}

/// The guard that catches a re-introduction.
///
/// **Splitting on `fn runserver` is not enough, and the first version of
/// this test did exactly that and passed with the bug reintroduced.**
/// The Postgres and non-Postgres arms live in the *same* function, so a
/// whole-body search finds `health_endpoints` in the Postgres arm and
/// reports the non-Postgres arm healthy. The property is per-`cfg`-arm,
/// so the test has to be too.
#[test]
fn the_non_postgres_runserver_arm_consults_health_endpoints() {
    let src = include_str!("../src/manage.rs");

    // The non-Postgres arm of `runserver`: from its cfg marker up to the
    // Postgres arm that follows it.
    let start = src
        .find("#[cfg(not(feature = \"postgres\"))]")
        .expect("runserver has a non-postgres arm");
    let rest = &src[start..];
    let end = rest
        .find("#[cfg(feature = \"postgres\")]\n        {")
        .expect("the postgres arm follows it");
    let non_pg_arm = &rest[..end];

    // Sanity: we really did capture a *serving* body, not a fragment.
    // Without this the assertion below could pass on an empty slice.
    assert!(
        non_pg_arm.contains("TcpListener::bind"),
        "the slice taken for the non-Postgres arm does not bind a listener, so the \
         markers have moved and this test is no longer looking at the serving \
         body. Fix the markers before trusting a pass."
    );

    assert!(
        non_pg_arm.contains("health_endpoints"),
        "the non-Postgres `runserver` arm never reads `self.health_endpoints`, so \
         `Cli::with_health()` is a silent no-op on every SQLite and MySQL build — \
         the flag is set, the server starts, and /health answers 404 (#1457)."
    );
    assert!(
        non_pg_arm.contains("health_router"),
        "the non-Postgres arm reads the flag but never merges `health_router`, \
         which leaves /health a 404 just as surely (#1457)."
    );
}
