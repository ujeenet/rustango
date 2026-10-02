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
    /// `auto_now_add` — `Unset` is what tells the macro to fill the
    /// column from the clock rather than honour a caller's value.
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

/// `now()` in a query must write a value that compares against a bound
/// timestamp, not merely emit a plausible-looking function call.
///
/// `date_functions.rs` asserts the SQL string. That is a proxy: it
/// proves the emitter changed, not that the value it produces can be
/// used. #1464's whole lesson is that the two are different questions —
/// the original format also looked right and could not be compared.
#[tokio::test]
async fn a_value_written_by_now_compares_against_a_bound_timestamp() {
    let pool = seeded().await;

    // Overwrite one row's timestamp through the ORM's `now()` — the
    // `SET col = now()` path — using the dialect's own emission.
    rustango::sql::raw_execute_pool(
        &pool,
        &format!(
            "UPDATE auto_now_add_fmt SET created_at = strftime('{}','now') WHERE label = 'a'",
            "%Y-%m-%dT%H:%M:%f000+00:00"
        ),
        Vec::new(),
    )
    .await
    .expect("set via now()");

    // Read that row back and bind it straight to an equality.
    let row: Vec<(DateTime<Utc>,)> = rustango::sql::raw_query_pool(
        "SELECT created_at FROM auto_now_add_fmt WHERE label = 'a'",
        Vec::new(),
        &pool,
    )
    .await
    .expect("read back");
    let hits: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT COUNT(*) FROM auto_now_add_fmt WHERE label = 'a' AND created_at = ?",
        vec![SqlValue::DateTime(row[0].0)],
        &pool,
    )
    .await
    .expect("equality");

    assert_eq!(
        hits[0].0,
        1,
        "a timestamp written by now() did not match itself when bound \
         back — the query path writes a shape the bind path cannot \
         reproduce. Stored: {:?}",
        stored_text(&pool).await
    );
}

/// The two format spellings must render the same bytes, checked by
/// running **both engines**.
///
/// `sql::sqlite::the_two_format_spellings_agree` compares chrono
/// against a hand-typed literal, because a unit test has no SQLite to
/// run `strftime` in. That leaves the strftime side unexercised: the
/// guard whose purpose is catching a divergence between the two
/// spellings never evaluates one of them (#1616 rework review,
/// tests-005). This closes that.
#[tokio::test]
async fn the_two_engines_render_the_same_bytes() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");

    // A whole-millisecond instant, the only precision both engines can
    // express: SQLite's `%f` is milliseconds, chrono's `%.6f` is
    // microseconds.
    let d: DateTime<Utc> = "2027-01-15T08:00:00.869Z".parse().expect("parse");

    // The SQLite side: strftime rendering that instant.
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT strftime('%Y-%m-%dT%H:%M:%f000+00:00', ?)",
        vec![SqlValue::String("2027-01-15 08:00:00.869".to_owned())],
        &pool,
    )
    .await
    .expect("strftime");
    let sqlite_side = &rows[0].0;

    // The Rust side: whatever the bind path writes for the same
    // instant. Read back through a round-trip so this is the bytes
    // that actually land in a column, not a formatting call.
    rustango::sql::raw_execute_pool(&pool, "CREATE TABLE r (v TEXT)", Vec::new())
        .await
        .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO r (v) VALUES (?)",
        vec![SqlValue::DateTime(d)],
    )
    .await
    .expect("insert");
    let back: Vec<(String,)> = rustango::sql::raw_query_pool("SELECT v FROM r", Vec::new(), &pool)
        .await
        .expect("read");
    let rust_side = &back[0].0;

    assert_eq!(
        sqlite_side, rust_side,
        "the DDL default and the bind path write different bytes for the \
         same instant. Everything in #1464 follows from these agreeing: a \
         column written by both holds two spellings and compares wrongly \
         across them."
    );
}

/// An **upgraded** table — one whose `DEFAULT` is still the legacy
/// `CURRENT_TIMESTAMP` — must still receive canonical rows.
///
/// This is the state no migration can repair. `CREATE TABLE IF NOT
/// EXISTS` leaves an existing table's default alone, and SQLite's
/// `ALTER TABLE` grammar is RENAME/ADD/DROP — there is no statement
/// that changes a column default. So every insert relying on that
/// default wrote the legacy shape forever, while the migrate sweep
/// converted the rows around it: the column went permanently mixed and
/// `ORDER BY` inverted the admin audit log (#1616 rework review,
/// correctness-001 / dialects-005).
///
/// `auto_now_add` binds from Rust now, so the stale default is never
/// reached. The fixture builds the table the **old** way on purpose — a
/// table created by the current DDL would pass whether or not the fix
/// is present.
#[tokio::test]
async fn an_upgraded_table_with_the_old_default_still_gets_canonical_rows() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");

    // The pre-#1464 shape, exactly as an upgraded database holds it.
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE auto_now_add_fmt (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            label TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
        Vec::new(),
    )
    .await
    .expect("create with the legacy default");

    // Inserted through the ORM, which is what an application does.
    let mut row = Event {
        id: Auto::Unset,
        label: "through-the-orm".to_owned(),
        created_at: Auto::Unset,
    };
    row.save_pool(&pool).await.expect("save");

    let stored = stored_text(&pool).await;
    assert_eq!(stored.len(), 1, "one row expected, got {stored:?}");
    assert!(
        stored[0].as_bytes()[10] == b'T' && stored[0].ends_with("+00:00"),
        "a row written through the ORM into a table whose DEFAULT is still \
         `CURRENT_TIMESTAMP` must carry the canonical shape — the default \
         must not be reached. Stored: {stored:?}"
    );

    // And it compares, which is the property the shape exists for.
    let hits: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT COUNT(*) FROM auto_now_add_fmt WHERE created_at < ?",
        vec![SqlValue::DateTime(Utc::now() + Duration::hours(1))],
        &pool,
    )
    .await
    .expect("compare");
    assert_eq!(
        hits[0].0, 1,
        "and it must compare against a bound timestamp"
    );
}
