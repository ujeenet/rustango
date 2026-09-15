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
//! ## This file has been wrong twice; read before editing
//!
//! **First version** split `manage.rs` on `fn runserver` and searched
//! the whole body. Both arms live in one function, so it found
//! `health_endpoints` in the *Postgres* arm and passed with the bug
//! reintroduced.
//!
//! **Second version** sliced the correct arm but substring-searched the
//! raw text — and the fix's own explanatory comment contains both
//! `health_endpoints` and `health_router`. Deleting the code and
//! leaving the comment passed all three tests. The revert that
//! "proved" it worked had deleted the comment too, by accident.
//!
//! So: strip comments first, then search. The property is about code.
//! `code_only` below is the whole point of the file.

#![cfg(all(feature = "admin", feature = "sqlite"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::sql::Pool;
use tower::ServiceExt;

/// Drop `//` comments so a search sees code, not prose.
///
/// Without this the guard matches the very comment that explains the
/// bug, which is how version two passed while `/health` 404'd.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The non-Postgres arm of `runserver`, comments removed.
fn non_postgres_runserver_arm() -> String {
    let src = include_str!("../src/manage.rs");
    let start = src
        .find("#[cfg(not(feature = \"postgres\"))]")
        .expect("runserver has a non-postgres arm");
    let rest = &src[start..];
    let end = rest
        .find("#[cfg(feature = \"postgres\")]\n        {")
        .expect("the postgres arm follows it");
    code_only(&rest[..end])
}

/// The guard. Delete the merge from the non-Postgres arm — with or
/// without its comment — and this fails.
#[test]
fn the_non_postgres_runserver_arm_mounts_the_health_router() {
    let arm = non_postgres_runserver_arm();

    // Prove the slice is the serving body, not an empty or shifted
    // fragment. Without this the assertions below could pass vacuously
    // if the markers moved.
    assert!(
        arm.contains("TcpListener::bind"),
        "the slice taken for the non-Postgres arm does not bind a listener, so the \
         markers have moved and this test is looking at the wrong text. Fix the \
         markers before trusting a pass.\n\n{arm}"
    );

    assert!(
        arm.contains("self.health_endpoints"),
        "the non-Postgres `runserver` arm never reads `self.health_endpoints`, so \
         `Cli::with_health()` is a silent no-op on every SQLite and MySQL build — \
         the flag is set, the server starts, and /health answers 404 (#1457).\n\n{arm}"
    );
    assert!(
        arm.contains("health::health_router"),
        "the non-Postgres arm reads the flag but never merges `health_router`, \
         which leaves /health a 404 just as surely (#1457).\n\n{arm}"
    );
}

/// Comment text must not be able to satisfy the guard.
///
/// This is the regression test *for the test* — it pins the exact
/// mistake version two made, so a future edit that drops `code_only`
/// fails here rather than silently going blind again.
#[test]
fn prose_alone_does_not_satisfy_the_guard() {
    let commented = "\
        // let api = if self.health_endpoints {\n\
        //     api.merge(crate::health::health_router(pool.clone()))\n\
        // };\n\
        let x = 1;";
    let stripped = code_only(commented);
    assert!(
        !stripped.contains("health_endpoints"),
        "`code_only` must remove commented-out code; leaving it is what let the \
         guard pass with the fix deleted. Got: {stripped:?}"
    );
    assert!(
        !stripped.contains("health_router"),
        "`code_only` must remove commented-out code. Got: {stripped:?}"
    );
}

// ---------------------------------------------------------------------
// Preconditions, not the guard.
//
// These two prove `health_router` works when built from a `Pool` enum
// rather than a driver-typed pool — a necessary condition for the fix,
// and the reason the fix is a one-line merge rather than new plumbing.
// They cannot fail on #1457 itself: they never go near `runserver`.
// Labelled so nobody reads a green here as "the mount works".
// ---------------------------------------------------------------------

async fn health_app() -> axum::Router {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::health::health_router(pool)
}

#[tokio::test]
async fn precondition_health_router_builds_from_a_sqlite_pool() {
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
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn precondition_ready_builds_from_a_sqlite_pool() {
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
    assert_eq!(res.status(), StatusCode::OK);
}
