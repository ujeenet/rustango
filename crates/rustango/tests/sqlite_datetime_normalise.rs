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
    /// A `Date`, declared on the model **on purpose**.
    ///
    /// `a_bare_date_column_is_not_rewritten` used to build its own
    /// table with a `born_on` column that belonged to no model, so the
    /// registry never saw it and the exclusion it claimed to test was
    /// unreachable: making `targets()` sweep `Date` columns left all 14
    /// tests green. Declaring it here puts the column in the registry,
    /// which is what makes the exclusion observable.
    pub born_on: chrono::NaiveDate,
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
        // `born_on` is a `Date` on the model and therefore NOT NULL.
        // Seeded with a bare date, which is also the control: the
        // sweep must leave it alone, and it is in the registry so it
        // would be reached if `targets()` stopped excluding `Date`.
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO normalise_evt (label, created_at, born_on) VALUES (?, ?, ?)",
            vec![
                SqlValue::String(label.to_owned()),
                SqlValue::String(ts.to_owned()),
                SqlValue::String("2026-09-19".to_owned()),
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
        "INSERT INTO normalise_evt (label, created_at, born_on) VALUES (?, ?, ?)",
        vec![
            SqlValue::String("mid".to_owned()),
            SqlValue::DateTime(mid),
            SqlValue::String("2026-09-19".to_owned()),
        ],
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
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_non_sqlite_pool_is_untouched() {
    // A **Postgres** pool, which is the branch this test names.
    //
    // It used a SQLite pool, on the stated grounds that "constructing a
    // PG pool needs a server". That is false — `connect_lazy` opens
    // nothing until the first query, and the dialect check returns
    // before any query is issued. So the test exercised the sqlite
    // path while claiming to cover the non-sqlite one, and removing
    // the early return entirely left all 14 tests in this file green
    // (#1616 review, tests-002).
    //
    // Gated on `postgres` because that is the honest scope: in a
    // sqlite-only build a non-sqlite pool cannot be constructed at all
    // (`Pool::connect_lazy` returns `FeatureNotEnabled`), so the branch
    // does not exist there and a test pretending to cover it would be
    // the same mistake in a new place. It runs wherever both backends
    // are linked, which includes `postgres_test`'s `--all-features`.
    let pool = Pool::connect_lazy("postgres://u:p@127.0.0.1:1/never_connected")
        .expect("lazy pool needs no server");
    assert_ne!(
        pool.dialect().name(),
        "sqlite",
        "fixture must be non-sqlite"
    );

    let out = normalise_sqlite_datetimes(&pool).await.expect("sweep");

    assert!(out.is_clean(), "a non-sqlite pool must rewrite nothing");
    assert_eq!(
        out.missing, 0,
        "the sweep must return before it looks for a single table — a \
         non-zero `missing` would mean it issued queries against a \
         database whose datetimes are a real type and were never broken"
    );
    assert!(
        out.columns.is_empty(),
        "and it must report no swept columns: {:?}",
        out.columns
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
        // Built from the model's own schema, so `born_on` is the
        // registry's `Date` column rather than one this test invented.
        // With a hand-rolled table the exclusion was untestable: the
        // registry never saw the column, so sweeping `Date` columns
        // could not have touched it and the assertion below held
        // whatever `targets()` did.
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

    // And the row actually changed. The report is a claim the sweep
    // makes about itself; asserting only that leaves a sweep which
    // lists the column and rewrites nothing indistinguishable from one
    // that works (#1616 rework review, tests-007).
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT occurred_at FROM rustango_audit_log",
        Vec::new(),
        &pool,
    )
    .await
    .expect("read back");
    assert_eq!(
        rows[0].0, "2026-09-19T12:00:00.000000+00:00",
        "the audit log row was reported as swept but still holds the \
         legacy shape"
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

/// A row the **fixed bind path** wrote must survive the sweep exactly.
///
/// This is the defect that made the first predicate unusable. It asked
/// `col <> strftime(FMT, col)`, which reads as "not already canonical"
/// and is not: SQLite's `%f` is milliseconds, `encode_datetime`'s
/// chrono `%.6f` is microseconds, so `strftime` is not the identity on
/// a canonical value — `.413681` renders back as `.414000`. Roughly 999
/// in 1000 correct rows matched, and every `migrate` rewrote them,
/// rounding each one forward and reporting it as a legacy conversion
/// (#1616 rework review, correctness-002 / dialects-001).
///
/// The microseconds are the whole point of the fixture. A whole-
/// millisecond value passes against the broken predicate too.
#[tokio::test]
async fn a_microsecond_value_is_not_touched_by_the_sweep() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::testkit::create_tables_for::<Evt>(&pool)
        .await
        .expect("create");

    // Sub-millisecond digits that SQLite's strftime cannot express.
    let precise: DateTime<Utc> = "2026-09-20T08:00:00.413681Z".parse().expect("parse");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO normalise_evt (label, created_at, born_on) VALUES (?, ?, ?)",
        vec![
            SqlValue::String("precise".to_owned()),
            SqlValue::DateTime(precise),
            SqlValue::String("2026-09-20".to_owned()),
        ],
    )
    .await
    .expect("insert");

    let before = stored(&pool).await;
    let out = normalise_sqlite_datetimes(&pool).await.expect("sweep");
    let after = stored(&pool).await;

    assert_eq!(
        before, after,
        "the sweep rewrote a row the bind path had already written \
         correctly. `strftime` is not the identity on a microsecond \
         value, so it must not be used to decide what is canonical."
    );
    assert!(
        out.is_clean(),
        "and it should report no work: {} row(s), {:?}",
        out.rows,
        out.columns
    );
    assert!(
        after[0].ends_with(".413681+00:00"),
        "the microseconds must survive untouched, got {}",
        after[0]
    );
}

/// A declared column whose migration has not run yet must be skipped,
/// not fatal.
///
/// `targets()` is built from the model registry, so a new `DateTime`
/// field is a sweep target the moment the binary is built — before the
/// migration adding it has been applied. SQLite answers that with a
/// **prepare** error, `no such column: <name>`, which the classifier
/// matched only for tables. The sweep returned `Err`, and because it
/// runs at the *end* of `migrate`, the run aborted **after earlier
/// migrations had already committed** (#1616 review, correctness-002 —
/// carried unfixed into the rework and handed over by four peers).
///
/// The table exists here and the column does not. A fixture with no
/// table at all passes against the old classifier too, so it would
/// have proved nothing.
#[tokio::test]
async fn a_declared_column_that_does_not_exist_yet_is_skipped() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");

    // The model's table, minus the column the model declares — exactly
    // the window between deploying code and applying its migration.
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE normalise_evt (id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT NOT NULL)",
        Vec::new(),
    )
    .await
    .expect("create table without created_at");

    let out = normalise_sqlite_datetimes(&pool)
        .await
        .expect("a column that does not exist yet must not fail the sweep");

    assert!(
        out.missing > 0,
        "the absent column should be counted as missing, got {out:?}"
    );
    assert!(out.is_clean(), "and nothing should have been rewritten");
}
