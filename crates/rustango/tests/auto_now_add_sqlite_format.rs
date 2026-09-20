//! `auto_now_add` on `SQLite` must store a timestamp that a Rust-bound
//! `DateTime<Utc>` can be compared against (#1464).
//!
//! The column is written by `SQLite`'s own `CURRENT_TIMESTAMP`, because
//! `auto_now_add` requires `Auto<..>` so the macro skips the column on
//! INSERT and the DB `DEFAULT` fires. `SQLite`'s `CURRENT_TIMESTAMP`
//! emits `YYYY-MM-DD HH:MM:SS`; sqlx encodes a `DateTime<Utc>` bind as
//! RFC3339. The two diverge at position 10 — `' '` (0x20) against
//! `'T'` (0x54) — so on a TEXT column, which compares
//! lexicographically, `WHERE col < ?` is **true for every row** no
//! matter what is bound.
//!
//! Found by the commerce soak running cursor pagination on
//! `Order.placed_at`: Postgres and `MySQL` paged correctly, both `SQLite`
//! legs returned page one forever.
//!
//! ## Why the existing suites do not catch it
//!
//! `cursor_pagination_on_a_timestamp.rs` hand-writes its DDL with no
//! `DEFAULT` and inserts Rust-written timestamps, so both sides of the
//! comparison are RFC3339 and agree. The issue predicted exactly that:
//! "a fixture whose timestamps are distinct and Rust-written will pass
//! while the bug is fully present". This suite therefore does two
//! things differently, and both are load-bearing:
//!
//! 1. the table is built by `testkit::create_tables_for`, the real DDL
//!    path, so the `DEFAULT` is whatever the framework actually emits;
//! 2. the rows are inserted **without** the timestamp, so the value
//!    under test is the one the database wrote, not one Rust chose.
//!
//! A version of this file that seeds the column explicitly passes
//! against the unfixed code. That is the trap, and it is the reason
//! the assertions below read the stored text directly rather than
//! trusting a round-trip.

#![cfg(feature = "sqlite")]

use chrono::{DateTime, Duration, Utc};
use rustango::core::SqlValue;
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "auto_now_add_fmt", display = "label")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub label: String,
    /// The column under test. `Auto<..>` is mandatory for
    /// `auto_now_add` — it is what makes the macro skip the column on
    /// INSERT so the DB default fires.
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

async fn seeded() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::testkit::create_tables_for::<Event>(&pool)
        .await
        .expect("create table from the model's own schema");

    for label in ["a", "b", "c"] {
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO auto_now_add_fmt (label) VALUES (?)",
            vec![SqlValue::String(label.to_owned())],
        )
        .await
        .expect("insert");
    }
    pool
}

/// Read the column as raw text, bypassing any decode that would hide
/// the stored shape.
async fn stored_text(pool: &Pool) -> Vec<String> {
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT CAST(created_at AS TEXT) FROM auto_now_add_fmt ORDER BY id",
        Vec::new(),
        pool,
    )
    .await
    .expect("select");
    rows.into_iter().map(|(s,)| s).collect()
}

/// The defect, stated as the property that is violated: a timestamp
/// the database wrote must sort against one Rust binds.
///
/// This is the assertion that fails on unfixed code. It does not
/// mention a format — a fix that chooses a different encoding than the
/// one this file expects is still correct as long as the comparison
/// works, and pinning the literal shape would forbid that.
#[tokio::test]
async fn a_db_written_timestamp_compares_against_a_rust_bound_one() {
    let pool = seeded().await;

    // Every row was written moments ago, so all three are strictly
    // before a cutoff an hour from now and none is before an hour ago.
    let future = Utc::now() + Duration::hours(1);
    let past = Utc::now() - Duration::hours(1);

    let before_future = count_before(&pool, future).await;
    let before_past = count_before(&pool, past).await;

    assert_eq!(
        before_future,
        3,
        "all three rows are older than a cutoff one hour in the future, \
         but `WHERE created_at < ?` matched {before_future}. Stored: {:?}",
        stored_text(&pool).await
    );
    assert_eq!(
        before_past,
        0,
        "no row is older than a cutoff one hour in the past, but \
         `WHERE created_at < ?` matched {before_past} — this is the \
         #1464 signature: the comparison is true for every row whatever \
         is bound. Stored: {:?}",
        stored_text(&pool).await
    );
}

/// **The property every other assertion in this file assumes and none of
/// them checks: a stored value must equal itself after a round trip.**
///
/// Read a timestamp out of the database, bind it straight back, and ask
/// for the row by equality. If the bind path and the write path encode
/// the same instant differently, this matches nothing.
///
/// That is not hypothetical — it is how the first version of this fix
/// failed. The DDL default wrote a fixed 6-digit fraction while sqlx
/// encoded a bound `DateTime<Utc>` with chrono's `AutoSi`, which emits
/// 0, 3, 6 or 9 digits by value. `...869000+00:00` and `...869+00:00`
/// are the same instant and different text, so equality found nothing
/// and `>` found the row itself — which is exactly #1464's
/// never-terminating cursor, reintroduced by its own fix.
///
/// Every other test here compares with `<` or `ORDER BY`, and all of
/// them pass while this property is broken. Ordering across *distinct*
/// instants is a different claim from identity of *one* instant, and
/// only the second one makes a cursor terminate.
#[tokio::test]
async fn a_stored_timestamp_equals_itself_after_a_round_trip() {
    let pool = seeded().await;

    // Decode what the database wrote...
    let decoded: Vec<(DateTime<Utc>,)> = rustango::sql::raw_query_pool(
        "SELECT created_at FROM auto_now_add_fmt ORDER BY id",
        Vec::new(),
        &pool,
    )
    .await
    .expect("decode");
    assert_eq!(decoded.len(), 3, "fixture should hold three rows");

    // ...and bind it straight back, unchanged.
    for (i, (d,)) in decoded.iter().enumerate() {
        let hits: Vec<(i64,)> = rustango::sql::raw_query_pool(
            "SELECT COUNT(*) FROM auto_now_add_fmt WHERE created_at = ?",
            vec![SqlValue::DateTime(*d)],
            &pool,
        )
        .await
        .expect("equality probe");
        assert!(
            hits[0].0 >= 1,
            "row {i}: a value read from the database did not match itself when \
             bound back. The write path and the bind path encode the same \
             instant differently. Stored: {:?}",
            stored_text(&pool).await
        );
    }
}

/// The cursor consequence, stated as the thing a caller actually does.
///
/// Paging with `WHERE col > <last row seen>` must not return that same
/// row again. Separate from the equality test above because this is the
/// user-visible symptom, and a fix could in principle satisfy one and
/// not the other.
#[tokio::test]
async fn a_cursor_does_not_re_emit_its_own_last_row() {
    let pool = seeded().await;

    let last: Vec<(i64, DateTime<Utc>)> = rustango::sql::raw_query_pool(
        "SELECT id, created_at FROM auto_now_add_fmt ORDER BY created_at DESC, id DESC LIMIT 1",
        Vec::new(),
        &pool,
    )
    .await
    .expect("last row");
    let (last_id, cursor) = last[0];

    let after: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT id FROM auto_now_add_fmt WHERE created_at > ? ORDER BY created_at, id",
        vec![SqlValue::DateTime(cursor)],
        &pool,
    )
    .await
    .expect("page two");

    assert!(
        !after.iter().any(|(id,)| *id == last_id),
        "the cursor re-emitted the row it was built from (id {last_id}) — this is \
         the #1464 non-terminating page. Returned: {:?}. Stored: {:?}",
        after.iter().map(|(i,)| *i).collect::<Vec<_>>(),
        stored_text(&pool).await
    );
}

async fn count_before(pool: &Pool, cutoff: DateTime<Utc>) -> i64 {
    let rows: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT COUNT(*) FROM auto_now_add_fmt WHERE created_at < ?",
        vec![SqlValue::DateTime(cutoff)],
        pool,
    )
    .await
    .expect("count");
    rows[0].0
}

/// The mixed-format half. A database that already holds both shapes
/// sorts wrongly across them, so `ORDER BY` is broken even with no
/// bind involved — which is why a bind-side-only fix is not sufficient
/// on an existing database.
///
/// Separate from the test above on purpose: that one can be satisfied
/// by the bind path alone, and this one cannot.
#[tokio::test]
async fn rows_written_by_the_db_and_by_rust_sort_together() {
    let pool = seeded().await;

    // A row written the way application code writes one: an explicit
    // Rust value, one hour older than the three the DEFAULT wrote.
    let older = Utc::now() - Duration::hours(1);
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO auto_now_add_fmt (label, created_at) VALUES (?, ?)",
        vec![
            SqlValue::String("rust-written".to_owned()),
            SqlValue::DateTime(older),
        ],
    )
    .await
    .expect("insert with an explicit timestamp");

    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT label FROM auto_now_add_fmt ORDER BY created_at ASC",
        Vec::new(),
        &pool,
    )
    .await
    .expect("ordered select");
    let order: Vec<String> = rows.into_iter().map(|(s,)| s).collect();

    assert_eq!(
        order.first().map(String::as_str),
        Some("rust-written"),
        "the explicitly-written row is an hour older than the other \
         three and must sort first, but the order was {order:?}. Stored: \
         {:?}",
        stored_text(&pool).await
    );
}
