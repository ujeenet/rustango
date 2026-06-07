#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted date-part WHERE
//! shortcuts: `where_year_pool` / `where_month_pool` /
//! `where_day_pool` / `where_hour_pool` / `where_minute_pool`.
//! Eloquent `whereYear` / `whereMonth` / `whereDay` / `whereHour`
//! / `whereMinute` parity.

use chrono::{TimeZone, Utc};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mdp_event")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub label: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mdp_event (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            label TEXT NOT NULL,
            at    TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    let rows = [
        ("a", Utc.with_ymd_and_hms(2024, 1, 15, 9, 30, 0).unwrap()),
        ("b", Utc.with_ymd_and_hms(2024, 6, 20, 10, 45, 0).unwrap()),
        ("c", Utc.with_ymd_and_hms(2025, 1, 15, 11, 30, 0).unwrap()),
        ("d", Utc.with_ymd_and_hms(2025, 12, 1, 12, 0, 0).unwrap()),
    ];
    for (label, at) in rows {
        let mut e = Event {
            id: Auto::default(),
            label: label.into(),
            at,
        };
        e.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_year_pool_filters_by_year() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Event::where_year_pool("at", 2024, &pool).await.unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&"a"));
    assert!(labels.contains(&"b"));
}

#[tokio::test]
async fn where_month_pool_filters_by_month() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Event::where_month_pool("at", 1, &pool).await.unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    // a (Jan 2024) + c (Jan 2025) — both January.
    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&"a"));
    assert!(labels.contains(&"c"));
}

#[tokio::test]
async fn where_day_pool_filters_by_day() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Event::where_day_pool("at", 15, &pool).await.unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&"a"));
    assert!(labels.contains(&"c"));
}

#[tokio::test]
async fn where_hour_pool_filters_by_hour() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Event::where_hour_pool("at", 10, &pool).await.unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0], "b");
}

#[tokio::test]
async fn where_minute_pool_filters_by_minute() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Event::where_minute_pool("at", 30, &pool).await.unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&"a"));
    assert!(labels.contains(&"c"));
}
