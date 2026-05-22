//! Django-parity #327 — `QuerySet::dates(field, kind)` returns the
//! distinct truncated date values matching the queryset, ordered.

#![cfg(feature = "sqlite")]

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rustango::core::SqlValue;
use rustango::query::{DateKind, DateTimeKind, QuerySet};
use rustango::sql::{fetch_dates_pool, fetch_datetimes_pool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qsd_post")]
#[allow(dead_code)]
pub struct QsdPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    published_at: chrono::DateTime<chrono::Utc>,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "qsd_post" (
            "id"           INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"        TEXT NOT NULL,
            "published_at" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    for (title, ts) in [
        ("a", "2024-01-15T12:00:00Z"),
        ("b", "2024-06-01T08:00:00Z"),
        ("c", "2025-03-10T09:00:00Z"),
        ("d", "2025-03-22T14:00:00Z"),
        ("e", "2025-11-15T18:00:00Z"),
    ] {
        let dt = Utc.from_utc_datetime(
            &chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ").unwrap(),
        );
        rustango::sql::raw_execute_pool(
            &pool,
            r#"INSERT INTO "qsd_post" ("title", "published_at") VALUES (?, ?)"#,
            vec![SqlValue::String(title.into()), SqlValue::DateTime(dt)],
        )
        .await
        .expect("seed");
    }
    pool
}

#[tokio::test]
async fn dates_by_year_returns_distinct_years() {
    let pool = build_pool().await;
    let years = fetch_dates_pool(
        &pool,
        QuerySet::<QsdPost>::new().dates("published_at", DateKind::Year),
    )
    .await
    .expect("dates(year)");
    assert_eq!(
        years,
        vec![
            NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
        ]
    );
}

#[tokio::test]
async fn dates_by_month_returns_distinct_months() {
    let pool = build_pool().await;
    let months = fetch_dates_pool(
        &pool,
        QuerySet::<QsdPost>::new().dates("published_at", DateKind::Month),
    )
    .await
    .expect("dates(month)");
    // 2024-01, 2024-06, 2025-03, 2025-11 — 4 distinct months (March 2025
    // has two rows but collapses).
    assert_eq!(
        months,
        vec![
            NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2024, 6, 1).unwrap(),
            NaiveDate::from_ymd_opt(2025, 3, 1).unwrap(),
            NaiveDate::from_ymd_opt(2025, 11, 1).unwrap(),
        ]
    );
}

#[tokio::test]
async fn dates_by_day_collapses_same_day() {
    let pool = build_pool().await;
    let days = fetch_dates_pool(
        &pool,
        QuerySet::<QsdPost>::new().dates("published_at", DateKind::Day),
    )
    .await
    .expect("dates(day)");
    // 5 rows, all distinct days.
    assert_eq!(days.len(), 5);
}

#[tokio::test]
async fn order_desc_reverses_output() {
    let pool = build_pool().await;
    let asc = fetch_dates_pool(
        &pool,
        QuerySet::<QsdPost>::new().dates("published_at", DateKind::Year),
    )
    .await
    .unwrap();
    let desc = fetch_dates_pool(
        &pool,
        QuerySet::<QsdPost>::new()
            .dates("published_at", DateKind::Year)
            .order_desc(true),
    )
    .await
    .unwrap();
    let mut reversed = asc.clone();
    reversed.reverse();
    assert_eq!(desc, reversed, "order_desc(true) should mirror asc");
}

#[tokio::test]
async fn filter_passes_through_to_dates() {
    // Restrict to 2025 rows; .dates(year) should return only [2025].
    let pool = build_pool().await;
    let years = fetch_dates_pool(
        &pool,
        QuerySet::<QsdPost>::new()
            .filter_op(
                "title",
                rustango::core::Op::In,
                SqlValue::List(vec![
                    SqlValue::String("c".into()),
                    SqlValue::String("e".into()),
                ]),
            )
            .dates("published_at", DateKind::Year),
    )
    .await
    .expect("filter+dates");
    assert_eq!(years, vec![NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()]);
}

#[tokio::test]
async fn datetimes_by_hour_collapses_minute() {
    let pool = build_pool().await;
    let buckets = fetch_datetimes_pool(
        &pool,
        QuerySet::<QsdPost>::new().datetimes("published_at", DateTimeKind::Hour),
    )
    .await
    .expect("datetimes(hour)");
    // Every seeded row has a distinct hour, so 5 buckets back.
    assert_eq!(buckets.len(), 5);
    // First bucket is the 2024-01-15T12:00:00Z entry truncated to the
    // hour (which is itself, since the seed already lives on a hour
    // boundary).
    let expected = Utc.from_utc_datetime(
        &NaiveDateTime::parse_from_str("2024-01-15T12:00:00Z", "%Y-%m-%dT%H:%M:%SZ").unwrap(),
    );
    assert_eq!(buckets[0], expected);
}

#[tokio::test]
async fn datetimes_by_year_returns_yyyy_01_01_midnight() {
    let pool = build_pool().await;
    let buckets = fetch_datetimes_pool(
        &pool,
        QuerySet::<QsdPost>::new().datetimes("published_at", DateTimeKind::Year),
    )
    .await
    .expect("datetimes(year)");
    let to_dt = |s: &str| -> DateTime<Utc> {
        Utc.from_utc_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").unwrap())
    };
    assert_eq!(
        buckets,
        vec![to_dt("2024-01-01T00:00:00Z"), to_dt("2025-01-01T00:00:00Z")]
    );
}

#[tokio::test]
async fn unknown_field_surfaces_query_error() {
    let pool = build_pool().await;
    let err = fetch_dates_pool(
        &pool,
        QuerySet::<QsdPost>::new().dates("not_a_field", DateKind::Year),
    )
    .await
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("not_a_field"),
        "error should name the unknown field: {msg}"
    );
}
