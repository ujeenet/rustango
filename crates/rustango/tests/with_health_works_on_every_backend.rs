//! `Cli::with_health()` must mount `/health` and `/ready` on every
//! serving path, not only Postgres (#1457).
//!
//! ## This file was wrong three times; read before editing
//!
//! Every version searched the *source text* of `runserver` for the fix,
//! and every version was fooled differently:
//!
//! 1. **v1** split on `fn runserver` and searched the whole body. Both
//!    arms live in one function, so it found `health_endpoints` in the
//!    Postgres arm and passed with the bug reintroduced.
//! 2. **v2** sliced the correct arm but substring-searched raw text —
//!    and the fix's own explanatory comment contains both
//!    `health_endpoints` and `health_router`. Deleting the code and
//!    leaving the comment passed all three tests.
//! 3. **v3** stripped comments first, and still missed: `runserver` has
//!    **three** serving paths, not two. The third is the `!pg_scheme`
//!    fallback *inside* the Postgres arm (#560), taken by a
//!    multi-backend build on a `sqlite://` or `mysql://` URL — exactly
//!    what the soak fleet's own `--features postgres,mysql,sqlite`
//!    image runs. The slice ended at the `#[cfg(feature = "postgres")]`
//!    marker, so it could not see that path at all, and two of the six
//!    soak instances went on answering 404.
//!
//! The lesson each time was the same one: **the property is that a
//! request gets a 200**, and none of those versions ever issued a
//! request.
//!
//! So the behaviour is now pinned where behaviour lives — a unit test
//! in `manage.rs` (`assemble_app_tests`) builds the router and asks it
//! for `/health`. All three paths assemble through one function, so one
//! property test covers them by construction.
//!
//! What is left here is the *structural* invariant that makes that
//! true: every serving path goes through that one assembly. A fourth
//! path with its own hand-rolled router would pass the property test
//! and 404 in production — that is precisely how #1457 survived its
//! first fix — so it is checked, and checked as what it is.

#![cfg(all(feature = "admin", feature = "sqlite"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::sql::Pool;
use tower::ServiceExt;

/// Drop `//` comments so a search sees code, not prose.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `Cli::runserver`, comments removed.
///
/// Bounded by the next `async fn` rather than by a `#[cfg]` marker:
/// anchoring on a `cfg` is what made v3 blind to the path nested inside
/// one.
fn runserver_body() -> String {
    let src = include_str!("../src/manage.rs");
    let start = src
        .find("async fn runserver(mut self)")
        .expect("Cli::runserver exists");
    let rest = &src[start..];
    let end = rest[1..]
        .find("\n    async fn ")
        .map_or(rest.len(), |i| i + 1);
    code_only(&rest[..end])
}

/// Every serving path assembles its router in one place.
///
/// A serving path is one that binds a listener. If some path binds
/// without having called `assemble_app`, it is building its own router
/// — and the next step it forgets will be silent, exactly as #1457 was.
#[test]
fn every_serving_path_goes_through_one_assembly() {
    let body = runserver_body();

    let binds = body.matches("TcpListener::bind").count();
    let assembles = body.matches("self.assemble_app(").count();

    assert!(
        binds > 0,
        "found no listener bind in `runserver`, so this test is reading the \
         wrong text — fix the markers before trusting a pass.\n\n{body}"
    );
    assert_eq!(
        assembles, binds,
        "`runserver` binds {binds} listener(s) but calls `assemble_app` \
         {assembles} time(s). A serving path that assembles its own router \
         will drift from the others — that is #1457, where one of three \
         paths never mounted the health endpoints and stayed 404 through \
         the first fix. Route the new path through `assemble_app`.\n\n{body}"
    );
}

/// And the health merge lives only in that assembly.
///
/// Re-inlining it into an arm would satisfy the count above while
/// putting the drift back.
#[test]
fn the_health_merge_is_not_duplicated_across_arms() {
    let src = code_only(include_str!("../src/manage.rs"));
    let merges = src.matches("health::health_router(").count();
    assert_eq!(
        merges, 1,
        "`health_router` is merged in {merges} places. One per serving path \
         is how #1457 happened: the fix reached two of the three. It belongs \
         in `assemble_app` only."
    );
}

// ---------------------------------------------------------------------
// Preconditions, not the guard.
//
// These prove `health_router` works when built from a `Pool` enum rather
// than a driver-typed pool — a necessary condition for the fix, and the
// reason it is a one-line merge rather than new plumbing. They cannot
// fail on #1457 itself: they never go near `runserver`. Labelled so
// nobody reads a green here as "the mount works".
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
