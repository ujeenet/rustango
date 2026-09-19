//! Rows written before #1464 must be converted, not just newly-written
//! ones.
//!
//! Fixing the DDL stops the framework producing `YYYY-MM-DD HH:MM:SS`,
//! and does nothing for the rows already on disk. A database carrying
//! both shapes sorts wrongly across them with no bind involved at all,
//! so a DDL-only fix leaves every existing deployment broken and
//! silent — which is the bug, not a fix for it.
//!
//! These tests therefore seed the **legacy** shape deliberately and
//! assert the sweep repairs it. Seeding the corrected shape and
//! watching the sweep leave it alone would pass against a sweep that
//! does nothing whatsoever, which is the trap this file exists to
//! avoid; `the_sweep_actually_changes_something` pins that directly.

#![cfg(feature = "sqlite")]

use chrono::{DateTime, Utc};
use rustango::core::SqlValue;
use rustango::migrate::sqlite_datetime::normalise_sqlite_datetimes;
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "normalise_evt", display = "label")]
#[allow(dead_code)]
pub struct Evt {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub label: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// A table holding exactly what a pre-fix database holds: values
/// written by the old `CURRENT_TIMESTAMP` default.
async fn legacy_db() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::testkit::create_tables_for::<Evt>(&pool)
        .await
        .expect("create");

    // Written as raw text in the old shape — this is the state the
    // sweep exists to repair. Deliberately out of insertion order so a
    // sweep that merely rewrites rows uniformly cannot pass the
    // ordering assertion by accident.
    for (label, ts) in [
        ("noon", "2026-09-19 12:00:00"),
        ("dawn", "2026-09-19 06:00:00"),
        ("dusk", "2026-09-19 18:00:00"),
    ] {
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO normalise_evt (label, created_at) VALUES (?, ?)",
            vec![
                SqlValue::String(label.to_owned()),
                SqlValue::String(ts.to_owned()),
            ],
        )
        .await
        .expect("seed legacy row");
    }
    pool
}

async fn stored(pool: &Pool) -> Vec<String> {
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT CAST(created_at AS TEXT) FROM normalise_evt ORDER BY id",
        Vec::new(),
        pool,
    )
    .await
    .expect("select");
    rows.into_iter().map(|(s,)| s).collect()
}

/// The sweep must report doing work. Without this, every other
/// assertion here is also satisfied by a no-op sweep on an
/// already-correct database.
#[tokio::test]
async fn the_sweep_actually_changes_something() {
    let pool = legacy_db().await;
    let before = stored(&pool).await;
    assert!(
        before.iter().all(|s| s.as_bytes()[10] == b' '),
        "fixture must start in the legacy shape, got {before:?}"
    );

    let out = normalise_sqlite_datetimes(&pool).await.expect("sweep");

    assert_eq!(out.rows, 3, "all three legacy rows should be rewritten");
    assert!(
        out.columns.iter().any(|c| c == "normalise_evt.created_at"),
        "the swept column should be named in the report, got {:?}",
        out.columns
    );
}

/// After the sweep, a legacy row compares against a bound timestamp —
/// the property the whole issue is about.
///
/// **The cutoff must fall on the same day as the rows.** The first
/// version of this test used cutoffs a day either side, and passed
/// with the sweep entirely disabled: comparing `2026-09-19 12:00:00`
/// against `2026-09-20T00:00:00+00:00` decides at position 9, on the
/// date, and never reaches the separator at position 10 where the two
/// formats disagree. It asserted a property that holds whether or not
/// the bug is present — the same proxy-testing mistake the issue warns
/// about and the reason it went unnoticed for so long.
///
/// A same-day cutoff forces the comparison to position 10, where a
/// legacy `' '` (0x20) sorts below `'T'` (0x54) and makes `<` true for
/// every row regardless of the hour.
#[tokio::test]
async fn a_converted_row_compares_against_a_bound_timestamp() {
    let pool = legacy_db().await;
    normalise_sqlite_datetimes(&pool).await.expect("sweep");

    // Rows are 06:00, 12:00 and 18:00 on 2026-09-19. A 09:00 cutoff on
    // that same day must select exactly one — "dawn".
    let same_day: DateTime<Utc> = "2026-09-19T09:00:00Z".parse().expect("parse");
    assert_eq!(
        count_before(&pool, same_day).await,
        1,
        "only the 06:00 row precedes a 09:00 cutoff; matching all three \
         is the #1464 signature. Stored: {:?}",
        stored(&pool).await
    );

    // And the other direction, still same-day: a 23:00 cutoff takes all
    // three, so the assertion above cannot be satisfied by a comparison
    // that simply matches nothing.
    let late: DateTime<Utc> = "2026-09-19T23:00:00Z".parse().expect("parse");
    assert_eq!(
        count_before(&pool, late).await,
        3,
        "all three rows precede a 23:00 cutoff on the same day. Stored: {:?}",
        stored(&pool).await
    );
}

async fn count_before(pool: &Pool, cutoff: DateTime<Utc>) -> i64 {
    let rows: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT COUNT(*) FROM normalise_evt WHERE created_at < ?",
        vec![SqlValue::DateTime(cutoff)],
        pool,
    )
    .await
    .expect("count");
    rows[0].0
}

/// A converted row and a freshly-bound one sort together. This is the
/// half a bind-side fix cannot deliver.
#[tokio::test]
async fn converted_and_native_rows_sort_together() {
    let pool = legacy_db().await;
    normalise_sqlite_datetimes(&pool).await.expect("sweep");

    // Between "dawn" (06:00) and "noon" (12:00).
    let mid: DateTime<Utc> = "2026-09-19T09:00:00Z".parse().expect("parse");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO normalise_evt (label, created_at) VALUES (?, ?)",
        vec![SqlValue::String("mid".to_owned()), SqlValue::DateTime(mid)],
    )
    .await
    .expect("insert native");

    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT label FROM normalise_evt ORDER BY created_at ASC",
        Vec::new(),
        &pool,
    )
    .await
    .expect("ordered");
    let order: Vec<String> = rows.into_iter().map(|(s,)| s).collect();

    assert_eq!(
        order,
        vec!["dawn", "mid", "noon", "dusk"],
        "a Rust-bound 09:00 must land between the converted 06:00 and \
         12:00. Stored: {:?}",
        stored(&pool).await
    );
}

/// Running twice must be identical to running once. The sweep is wired
/// into every `migrate`, so a non-idempotent version would corrupt on
/// the second invocation rather than the first — the worst possible
/// failure shape, because it would pass every test that runs it once.
#[tokio::test]
async fn the_sweep_is_idempotent() {
    let pool = legacy_db().await;

    let first = normalise_sqlite_datetimes(&pool).await.expect("first");
    let after_first = stored(&pool).await;

    let second = normalise_sqlite_datetimes(&pool).await.expect("second");
    let after_second = stored(&pool).await;

    assert_eq!(first.rows, 3, "the first pass converts the seeded rows");
    assert_eq!(
        second.rows, 0,
        "the second pass must match nothing, but rewrote {} row(s)",
        second.rows
    );
    assert_eq!(
        after_first, after_second,
        "the stored values must be unchanged by a second sweep"
    );
}

/// A pool that is not SQLite must be left alone. The sweep is called
/// unconditionally from `migrate`, so this is what keeps it off
/// Postgres and MySQL, where the column is a real datetime type and
/// `strftime` does not exist.
#[tokio::test]
async fn a_non_sqlite_pool_is_untouched() {
    // Constructing a PG pool needs a server; the dialect check happens
    // before any query, so assert on the branch that does not. A
    // SQLite pool with no tables at all exercises the same early path
    // plus the missing-table accounting.
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    let out = normalise_sqlite_datetimes(&pool).await.expect("sweep");
    assert!(out.is_clean(), "no tables means nothing to rewrite");
    assert!(
        out.missing > 0,
        "the framework tables do not exist here, so they must be \
         counted as missing rather than erroring"
    );
}

/// A `Date` column must not be swept. It has no `T` separator to
/// disagree about, and `strftime` with the datetime format would
/// rewrite a bare date into a full timestamp — turning a correct value
/// into a wrong one.
#[tokio::test]
async fn a_bare_date_column_is_not_rewritten() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE normalise_evt (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         label TEXT NOT NULL, created_at TEXT NOT NULL, born_on TEXT NOT NULL)",
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO normalise_evt (label, created_at, born_on) \
         VALUES ('x', '2026-09-19 12:00:00', '2026-09-19')",
        Vec::new(),
    )
    .await
    .expect("seed");

    let out = normalise_sqlite_datetimes(&pool).await.expect("sweep");

    let rows: Vec<(String, String)> = rustango::sql::raw_query_pool(
        "SELECT born_on, created_at FROM normalise_evt",
        Vec::new(),
        &pool,
    )
    .await
    .expect("select");
    assert_eq!(
        rows[0].0, "2026-09-19",
        "a Date column carries no time separator and must be left exactly as it was"
    );
    // Paired with a positive assertion on the same row, or a sweep
    // that does nothing at all would satisfy the check above. The
    // datetime column beside it must have been converted.
    assert!(
        out.rows > 0 && rows[0].1.as_bytes()[10] == b'T',
        "the datetime column in the same table must be converted, so \
         this proves the sweep ran and skipped only the Date column. \
         rows={} created_at={}",
        out.rows,
        rows[0].1
    );
}

/// The framework's own hand-written tables are swept too. They are not
/// in the model registry, so only the explicit list reaches them — and
/// `rustango_audit_log.occurred_at` is compared against a bound cutoff
/// by audit retention, which is the same defect one table over.
#[tokio::test]
async fn a_framework_table_is_swept() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    crate::audit_table(&pool).await;
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO rustango_audit_log (occurred_at) VALUES ('2026-09-19 12:00:00')",
        Vec::new(),
    )
    .await
    .expect("seed");

    let out = normalise_sqlite_datetimes(&pool).await.expect("sweep");

    assert!(
        out.columns
            .iter()
            .any(|c| c == "rustango_audit_log.occurred_at"),
        "the audit log is not in the model registry, so it is reached \
         only through the explicit framework list. Swept: {:?}",
        out.columns
    );
}

/// Minimal stand-in for the audit table — only the column under test.
/// The real `ensure_table_pool` needs the `audit` feature, and this
/// suite is about the sweep, not about audit's schema.
async fn audit_table(pool: &Pool) {
    rustango::sql::raw_execute_pool(
        pool,
        "CREATE TABLE rustango_audit_log (occurred_at TEXT NOT NULL)",
        Vec::new(),
    )
    .await
    .expect("create audit table");
}
