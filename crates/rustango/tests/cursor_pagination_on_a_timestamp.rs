//! Cursor pagination must work on a timestamp, and must refuse an
//! unusable column where it is configured (#1459).
//!
//! `cursor_pagination_desc("placed_at")` used to be accepted at build
//! time and then return **500 on every request**:
//!
//! ```text
//! {"error":"cursor pagination requires an integer field (i16/i32/i64)"}
//! ```
//!
//! Two defects in one. The restriction was undocumented — `docs/viewsets.md`
//! and the method's own doc comment said "a stable,
//! monotonically-ordered column (typically `id`)", which a `TIMESTAMPTZ`
//! is, and which is the canonical cursor in the DRF API this is shaped
//! after. And a configuration error surfaced as a server error, once per
//! request, forever: the ViewSet built, the process started, health
//! checks passed, and the endpoint was dead.
//!
//! Found by the commerce soak, which had pointed cursor pagination at an
//! order's `placed_at` for exactly the reason the docs describe.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{DateTime, Utc};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::viewset::ViewSet;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "cursor_ts_event", display = "label")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub label: String,
    /// The cursor column. A timestamp on an append-only table is the
    /// case cursor pagination exists to serve.
    pub occurred_at: DateTime<Utc>,
    /// Deliberately a float: nothing may accept it as a cursor, because
    /// floats do not round-trip exactly through a token.
    pub score: f64,
}

const DDL: &str = "CREATE TABLE cursor_ts_event (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    label       TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    score       REAL NOT NULL
)";

async fn seeded_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(&pool, DDL, Vec::new())
        .await
        .expect("create");
    for i in 0..7 {
        let sql = format!(
            "INSERT INTO cursor_ts_event (label, occurred_at, score) \
             VALUES ('e{i}', '2026-09-1{}T10:00:00+00:00', {}.5)",
            i + 1,
            i
        );
        rustango::sql::raw_execute_pool(&pool, &sql, Vec::new())
            .await
            .expect("insert");
    }
    pool
}

async fn get(pool: &Pool, uri: &str) -> (StatusCode, serde_json::Value) {
    let app = ViewSet::for_model(Event::SCHEMA)
        .cursor_pagination("occurred_at")
        .page_size(3)
        .router_pool("/events", pool.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn a_timestamp_cursor_answers_instead_of_500ing() {
    let pool = seeded_pool().await;
    let (status, body) = get(&pool, "/events").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a timestamp cursor must serve a page; it 500'd on every request before \
         #1459. Body: {body}"
    );
    assert_eq!(
        body["results"].as_array().map(Vec::len),
        Some(3),
        "page_size(3) should bound the page: {body}"
    );
    assert!(
        body["next"].is_string(),
        "seven rows at page_size 3 must offer a next cursor: {body}"
    );
}

/// The cursor has to *advance*, not just exist. A token that decodes to
/// the wrong type would still be a string in the response while paging
/// forever over page one.
#[tokio::test]
async fn the_timestamp_cursor_actually_walks_the_table() {
    let pool = seeded_pool().await;

    let (_, first) = get(&pool, "/events").await;
    let labels_1: Vec<String> = first["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap_or_default().to_owned())
        .collect();
    let token = first["next"].as_str().expect("next cursor").to_owned();

    let (status, second) = get(&pool, &format!("/events?cursor={token}")).await;
    assert_eq!(status, StatusCode::OK, "second page: {second}");
    let labels_2: Vec<String> = second["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap_or_default().to_owned())
        .collect();

    assert_eq!(labels_1, vec!["e0", "e1", "e2"], "first page, ascending");
    assert_eq!(
        labels_2,
        vec!["e3", "e4", "e5"],
        "the cursor must advance past the first page, not repeat it"
    );
}

/// Projecting the cursor column away must be loud, not silent — and
/// now it is loud at **build** time.
///
/// `.fields([..])` can omit the cursor column. The row then carries no
/// value to encode, and answering `next: null` while `has_more` is true
/// stops pagination at page one with nothing reporting an error: a
/// caller iterating `next` sees three rows and concludes that is the
/// table.
///
/// The first fix made that a 500 per request. This makes it a panic at
/// the call that creates it, which is the same move #1459 made for an
/// unusable cursor *type* — the field list and the cursor column are
/// both static properties of the builder.
#[test]
#[should_panic(expected = "projected away")]
fn projecting_the_cursor_column_away_is_refused_at_build_time() {
    let _ = ViewSet::for_model(Event::SCHEMA)
        .cursor_pagination("occurred_at")
        .fields(&["id", "label"]); // occurred_at deliberately absent
}

/// And in the other order: the projection can be narrowed after the
/// cursor is chosen. Without the check on both builder methods, one
/// ordering would be caught and the other would not.
#[test]
#[should_panic(expected = "projected away")]
fn the_check_does_not_depend_on_builder_order() {
    let _ = ViewSet::for_model(Event::SCHEMA)
        .fields(&["id", "label"])
        .cursor_pagination("occurred_at");
}

/// The primary key is required too — it breaks ties.
#[test]
#[should_panic(expected = "primary key")]
fn projecting_the_primary_key_away_is_refused() {
    let _ = ViewSet::for_model(Event::SCHEMA)
        .cursor_pagination("occurred_at")
        .fields(&["label", "occurred_at"]); // no `id`
}

/// A projection that keeps both is fine.
#[tokio::test]
async fn a_projection_containing_the_cursor_and_pk_still_serves() {
    let pool = seeded_pool().await;
    let app = ViewSet::for_model(Event::SCHEMA)
        .cursor_pagination("occurred_at")
        .fields(&["id", "label", "occurred_at"])
        .page_size(3)
        .router_pool("/events", pool.clone());

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["next"].is_string(),
        "a projection that keeps the cursor and the pk must still issue a token: {json}"
    );
}

/// A bad cursor is the caller's fault: 400, not 500.
#[tokio::test]
async fn a_malformed_cursor_is_a_client_error() {
    let pool = seeded_pool().await;
    let (status, _) = get(&pool, "/events?cursor=not-a-real-token").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The other half of #1459: an unusable column must fail where it is
/// **configured**, so the mistake is found at start-up rather than by
/// every request for the life of the deployment.
#[test]
#[should_panic(expected = "cannot be a cursor")]
fn a_float_column_is_refused_at_build_time() {
    let _ = ViewSet::for_model(Event::SCHEMA).cursor_pagination("score");
}

#[test]
#[should_panic(expected = "has no field")]
fn an_unknown_column_is_refused_at_build_time() {
    let _ = ViewSet::for_model(Event::SCHEMA).cursor_pagination("no_such_column");
}

// ---------------------------------------------------------------------
// Ties. The fixture above seeds seven timestamps a day apart, which is
// the shape that hides this: with every value distinct, a strict `>` on
// the cursor column alone is correct.
//
// A real append-only table is not like that. `bulk_insert` writes a run
// of rows with one timestamp; on Postgres every row in a transaction
// shares `now()`. When a page boundary lands inside such a run, asking
// for `occurred_at > v` skips every remaining row with that value —
// silently, because the response is a well-formed page with a
// well-formed `next`.
// ---------------------------------------------------------------------

/// Nine rows, three timestamps, three rows each. Page size 2 guarantees
/// boundaries fall *inside* a tie group rather than between groups.
async fn tied_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(&pool, DDL, Vec::new())
        .await
        .expect("create");
    for i in 0..9 {
        // The fraction is explicit because #1464 made the stored
        // spelling part of the contract: SQLite compares these as text,
        // and the canonical shape is a fixed six digits. This literal
        // was `T10:00:00+00:00` — the variable-width form sqlx used to
        // write, which is what a database from before that release
        // holds. That form no longer equals the value the ORM binds, so
        // the tiebreaker's `occurred_at = ?` leg missed and paging
        // skipped the rest of each tie group.
        //
        // Changed here rather than "fixed" in the code, because the
        // property under test is the tiebreaker and the old literal was
        // incidental to it. The upgrade path that old literal really
        // represents is covered by `old_shape_rows_page_correctly_after_the_sweep`
        // below, which seeds exactly that spelling on purpose.
        let sql = format!(
            "INSERT INTO cursor_ts_event (label, occurred_at, score) \
             VALUES ('e{i}', '2026-09-1{}T10:00:00.000000+00:00', {i}.5)",
            i / 3 + 1
        );
        rustango::sql::raw_execute_pool(&pool, &sql, Vec::new())
            .await
            .expect("insert");
    }
    pool
}

/// Walk the whole table two rows at a time and demand every row once.
///
/// This is the assertion that matters, and it is on the property rather
/// than on a page: paginating to exhaustion must visit all nine labels,
/// no more and no fewer. Before the tiebreaker it yields three.
#[tokio::test]
async fn paging_through_tied_timestamps_visits_every_row() {
    let pool = tied_pool().await;

    let page = |uri: String| {
        let pool = pool.clone();
        async move {
            let app = ViewSet::for_model(Event::SCHEMA)
                .cursor_pagination("occurred_at")
                .page_size(2)
                .router_pool("/events", pool);
            let res = app
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri(&uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("request");
            let status = res.status();
            let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .expect("body");
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    };

    let mut seen: Vec<String> = Vec::new();
    let mut uri = "/events".to_string();
    // Bounded so a cursor that fails to advance ends the test rather
    // than the process.
    for _ in 0..20 {
        let (status, body) = page(uri.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for row in body["results"].as_array().expect("results") {
            seen.push(row["label"].as_str().unwrap_or_default().to_owned());
        }
        match body["next"].as_str() {
            Some(t) => uri = format!("/events?cursor={t}"),
            None => break,
        }
    }

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        9,
        "paging to exhaustion must visit all nine rows; three timestamps x three \
         rows means every page boundary lands inside a tie group, and a strict \
         `occurred_at > v` skips the rest of the group. Saw {} row(s): {seen:?}",
        unique.len()
    );
    assert_eq!(
        seen.len(),
        9,
        "and must visit each exactly once — a non-strict `>=` would repeat the \
         boundary row forever instead. Saw: {seen:?}"
    );
}

/// The same, descending — the `Lt` arm is a separate branch.
#[tokio::test]
async fn paging_descending_through_ties_visits_every_row() {
    let pool = tied_pool().await;
    let mut seen: Vec<String> = Vec::new();
    let mut uri = "/events".to_string();

    for _ in 0..20 {
        let app = ViewSet::for_model(Event::SCHEMA)
            .cursor_pagination_desc("occurred_at")
            .page_size(2)
            .router_pool("/events", pool.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for row in body["results"].as_array().expect("results") {
            seen.push(row["label"].as_str().unwrap_or_default().to_owned());
        }
        match body["next"].as_str() {
            Some(t) => uri = format!("/events?cursor={t}"),
            None => break,
        }
    }

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        9,
        "descending must also visit all nine: {seen:?}"
    );
    assert_eq!(seen.len(), 9, "exactly once each: {seen:?}");
}

/// A pre-#1459 token — a bare base64 value with no tiebreaker — must
/// still be accepted, because a client can be mid-pagination across the
/// upgrade that introduced the composite form.
#[tokio::test]
async fn a_legacy_single_value_token_is_still_accepted() {
    use base64::Engine as _;
    let pool = seeded_pool().await;
    let legacy =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"2026-09-13T10:00:00+00:00");

    let (status, body) = get(&pool, &format!("/events?cursor={legacy}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a token issued before the tiebreaker must not 400: {body}"
    );
    assert!(
        body["results"].as_array().is_some_and(|r| !r.is_empty()),
        "and must still return the rows after it: {body}"
    );
}

// ---------------------------------------------------------------------
// The upgrade path: a database written before #1464 (2026-09-19).
//
// `tied_pool` above now seeds the canonical spelling, because the
// tiebreaker is what it tests. This seeds what a real pre-#1464
// database actually holds — sqlx's old variable-width RFC3339, where a
// whole-second instant carries no fractional part at all — and proves
// two things in order: that such a database pages *wrongly* until the
// sweep runs, and correctly afterwards.
//
// The first half matters as much as the second. Fixed-width comparison
// only works once every stored value is fixed width, so "run migrate"
// is a real precondition of this release and not a detail. A test that
// only checked the after state would let the precondition go unstated.
// ---------------------------------------------------------------------

/// Page cap for the walk below — high enough that a healthy cursor
/// finishes long before it, low enough that a stuck one ends the test
/// rather than the process.
const MAX_PAGES: usize = 20;

/// Nine rows in the pre-#1464 spelling, three per timestamp.
async fn legacy_shape_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(&pool, DDL, Vec::new())
        .await
        .expect("create");
    for i in 0..9 {
        // No fractional part — exactly what sqlx wrote for a
        // whole-second instant before this release.
        let sql = format!(
            "INSERT INTO cursor_ts_event (label, occurred_at, score) \
             VALUES ('e{i}', '2026-09-1{}T10:00:00+00:00', {i}.5)",
            i / 3 + 1
        );
        rustango::sql::raw_execute_pool(&pool, &sql, Vec::new())
            .await
            .expect("insert");
    }
    pool
}

async fn walk_all_labels(pool: &Pool) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut uri = "/events".to_string();
    let mut exhausted = false;
    for _ in 0..MAX_PAGES {
        let app = ViewSet::for_model(Event::SCHEMA)
            .cursor_pagination("occurred_at")
            .page_size(2)
            .router_pool("/events", pool.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .expect("body"),
        )
        .unwrap();
        for row in body["results"].as_array().expect("results") {
            seen.push(row["label"].as_str().unwrap_or_default().to_owned());
        }
        match body["next"].as_str() {
            Some(t) => uri = format!("/events?cursor={t}"),
            None => {
                exhausted = true;
                break;
            }
        }
    }

    // The walk must END, and must not repeat a row. Both were invisible
    // before: this returned `seen.sort(); seen.dedup()`, so a re-emitted
    // row and a cursor that never terminates — the two canonical #1464
    // symptoms — were erased by the helper before any assertion saw them
    // (#1616 rework review, tests-010).
    assert!(
        exhausted,
        "the cursor did not terminate within {MAX_PAGES} pages; it is \
         re-emitting rows rather than advancing, which is the #1464 \
         failure this fixture exists to detect. Saw: {seen:?}"
    );
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        seen.len(),
        "a row was emitted more than once: {seen:?}. A cursor that repeats \
         a row is the #1464 signature — the last row of a page compares as \
         'after' itself."
    );

    seen.sort();
    seen
}

#[tokio::test]
async fn old_shape_rows_page_correctly_after_the_sweep() {
    let pool = legacy_shape_pool().await;

    // Before: the stored values are not the shape the ORM now binds, so
    // the tiebreaker's equality leg misses and rows are skipped. This
    // asserts the precondition rather than glossing over it.
    let before = walk_all_labels(&pool).await;
    assert!(
        before.len() < 9,
        "this fixture is meant to start broken — pre-#1464 rows do not \
         compare against a fixed-width bind. Saw {} of 9: {before:?}. If \
         this now reaches 9, the sweep is no longer a precondition and \
         this test should be rewritten, not deleted.",
        before.len()
    );

    // The sweep is what `migrate` runs.
    let fixed = rustango::migrate::sqlite_datetime::normalise_sqlite_datetimes(&pool)
        .await
        .expect("sweep");
    assert!(
        fixed.rows >= 9,
        "the sweep should have rewritten all nine rows, rewrote {}",
        fixed.rows
    );

    // After: every row is reachable.
    let after = walk_all_labels(&pool).await;
    assert_eq!(
        after.len(),
        9,
        "after the sweep every row must be visited exactly once, saw {after:?}"
    );
}
