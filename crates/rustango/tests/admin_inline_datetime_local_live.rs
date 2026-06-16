//! Regression test for the editable-inline `datetime-local` rendering fix.
//!
//! An editable inline row carrying a `DateTime` field must render its
//! value in the format a `<input type="datetime-local">` accepts —
//! `YYYY-MM-DDTHH:MM:SS`, no timezone offset. The pre-fix inline path
//! stringified the raw JSON value, leaving the RFC3339 `+00:00` offset in
//! place, which browsers reject (the field rendered empty + invalid).
//!
//! Uses its own throwaway models so it can't perturb the other inline
//! tests' inventory.

#![cfg(feature = "postgres")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::{TimeZone, Utc};
use rustango::admin::inlines::InlineKind;
use rustango::register_admin_inline;
use rustango::sql::sqlx;
use rustango::sql::Auto;
use rustango::Model;
use tokio::sync::Mutex;
use tower::ServiceExt;

fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

#[derive(Model, Debug)]
#[rustango(table = "idl_event")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[derive(Model, Debug)]
#[rustango(table = "idl_session")]
#[allow(dead_code)]
pub struct Session {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "idl_event", on = "id")]
    pub event_id: i64,
    #[rustango(max_length = 200)]
    pub label: String,
    pub starts_at: chrono::DateTime<chrono::Utc>,
}

register_admin_inline!(
    parent = "idl_event",
    child = "idl_session",
    fk = "event_id",
    kind = InlineKind::Tabular,
    label = "Sessions",
    fields = &["label", "starts_at"],
);

async fn fresh(pool: &sqlx::PgPool) {
    for t in ["idl_session", "idl_event"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "idl_event" (id BIGSERIAL PRIMARY KEY, name VARCHAR(100) NOT NULL)"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "idl_session" (
               id BIGSERIAL PRIMARY KEY,
               event_id BIGINT NOT NULL REFERENCES "idl_event"(id),
               label VARCHAR(200) NOT NULL,
               starts_at TIMESTAMPTZ NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn inline_datetime_renders_in_datetime_local_format() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut event = Event {
        id: Auto::default(),
        name: "Conf".into(),
    };
    event.insert(&pool).await.unwrap();
    let event_id = *event.id.get().expect("PK assigned");

    let mut session = Session {
        id: Auto::default(),
        event_id,
        label: "Keynote".into(),
        starts_at: Utc.with_ymd_and_hms(2025, 1, 2, 13, 1, 0).unwrap(),
    };
    session.insert(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/idl_event/{event_id}/edit"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 2_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // The inline datetime input is rendered, prefix-mangled per FormSet.
    assert!(
        html.contains(r#"name="idl_session-0-starts_at""#),
        "inline starts_at input missing: {html}"
    );
    // It carries the truncated, offset-free value `datetime-local` accepts.
    assert!(
        html.contains(r#"value="2025-01-02T13:01:00""#),
        "expected offset-free datetime-local value: {html}"
    );
    // And specifically NOT the raw RFC3339 string with the TZ offset.
    assert!(
        !html.contains("2025-01-02T13:01:00+00:00"),
        "datetime-local value must not include a TZ offset: {html}"
    );
}
