#![cfg(feature = "sqlite")]
//! Live SQLite end-to-end test for the date-part field lookups
//! (`__year` / `__month` / `__day` / `__date` / `__week_day` /
//! `__hour`) on `.filter()` — issue #829.
//!
//! Emission already pinned by [`filter_date_lookup.rs`]; this file
//! proves the SQL the parser produces actually returns the expected
//! rows against SQLite via the existing `Extract*` / `TruncDate`
//! emitters.

use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fdll_event")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub label: String,
    pub created: chrono::DateTime<chrono::Utc>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE fdll_event (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            label   TEXT NOT NULL,
            created TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    // Three rows across three years and months, with explicit hours.
    for (label, ts) in [
        (
            "y2024-jan",
            Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap(),
        ),
        (
            "y2025-feb",
            Utc.with_ymd_and_hms(2025, 2, 20, 14, 30, 0).unwrap(),
        ),
        (
            "y2026-jun",
            Utc.with_ymd_and_hms(2026, 6, 6, 23, 59, 0).unwrap(),
        ),
    ] {
        let mut e = Event {
            id: Auto::default(),
            label: label.into(),
            created: ts,
        };
        e.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn year_lookup_matches_only_target_year() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Event> = QuerySet::<Event>::default()
        .filter("created__year", 2025_i64)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "only y2025 should match: {rows:?}");
    assert_eq!(rows[0].label, "y2025-feb");
}

#[tokio::test]
async fn year_lookup_gte_picks_recent_years() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Event> = QuerySet::<Event>::default()
        .filter("created__year__gte", 2025_i64)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let labels: Vec<&str> = rows.iter().map(|e| e.label.as_str()).collect();
    assert!(labels.contains(&"y2025-feb"));
    assert!(labels.contains(&"y2026-jun"));
}

#[tokio::test]
async fn month_lookup_matches_by_month_only() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Event> = QuerySet::<Event>::default()
        .filter("created__month", 2_i64)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "y2025-feb");
}

#[tokio::test]
async fn day_lookup_matches_by_day_of_month() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Event> = QuerySet::<Event>::default()
        .filter("created__day", 6_i64)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "y2026-jun");
}

#[tokio::test]
async fn hour_lookup_matches_by_hour() {
    let pool = make_pool().await;
    seed(&pool).await;

    let rows: Vec<Event> = QuerySet::<Event>::default()
        .filter("created__hour", 14_i64)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "y2025-feb");
}

#[tokio::test]
async fn date_lookup_matches_full_date() {
    let pool = make_pool().await;
    seed(&pool).await;

    let target = NaiveDate::from_ymd_opt(2026, 6, 6).unwrap();
    let rows: Vec<Event> = QuerySet::<Event>::default()
        .filter("created__date", target)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "y2026-jun");
}

#[tokio::test]
async fn week_day_lookup_matches_normalized_dow() {
    let pool = make_pool().await;
    seed(&pool).await;

    // 2024-01-15 is a Monday → strftime('%w') = '1'
    // 2025-02-20 is a Thursday → '4'
    // 2026-06-06 is a Saturday → '6'
    let rows: Vec<Event> = QuerySet::<Event>::default()
        .filter("created__week_day", 6_i64)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "y2026-jun");
}

#[tokio::test]
async fn quarter_lookup_errors_on_sqlite() {
    let pool = make_pool().await;
    seed(&pool).await;

    let result = QuerySet::<Event>::default()
        .filter("created__quarter", 2_i64)
        .fetch_pool(&pool)
        .await;
    assert!(
        result.is_err(),
        "SQLite has no native quarter; should error consistently"
    );
}

// Smoke: keep an unused chrono ref so warnings stay clean if cfg
// trims arms in the future.
#[allow(dead_code)]
fn _smoke(_: NaiveDateTime) {}
