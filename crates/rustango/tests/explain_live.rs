//! `QuerySet::explain` helper — closes future-backlog item #5
//! ("ORM query profiling / EXPLAIN").
//!
//! Each test seeds a tiny table, runs a queryset against it, asks for
//! the planner output, and asserts the plan body looks plausible.
//! `ANALYZE` is opt-in (it actually executes the query) — verified
//! by spotting the `actual time=` token Postgres only emits when the
//! plan was executed.

#![cfg(feature = "tenancy")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{sqlx, Auto, ExplainFormat, ExplainOptions};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "_explain_demo")]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "_explain_demo" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "_explain_demo" (
            "id"    BIGSERIAL    PRIMARY KEY,
            "label" VARCHAR(64)  NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    for label in ["alpha", "beta", "gamma"] {
        let mut d = Demo {
            id: Auto::default(),
            label: label.into(),
        };
        d.insert(pool).await.unwrap();
    }
}

#[tokio::test]
async fn explain_returns_plan_lines() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let plan = Demo::objects()
        .where_(Demo::label.eq("alpha"))
        .explain(&pool)
        .await
        .unwrap();

    assert!(!plan.is_empty(), "EXPLAIN should return at least one line");
    let joined = plan.join("\n");
    // Every plan line starts with one of these node tokens. Postgres
    // chooses Seq Scan vs Index Scan based on stats; we just want to
    // confirm we got real planner output, not an empty string.
    assert!(
        joined.contains("Scan") || joined.contains("Filter") || joined.contains("Plan"),
        "expected planner output, got: {joined}"
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "_explain_demo" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn explain_with_analyze_reports_actual_timings() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let plan = Demo::objects()
        .explain_on(
            &pool,
            ExplainOptions {
                analyze: true,
                buffers: true,
                verbose: false,
                format: ExplainFormat::Text,
            },
        )
        .await
        .unwrap();
    let joined = plan.join("\n");
    assert!(
        joined.contains("actual time="),
        "ANALYZE output should include actual-timing column: {joined}"
    );
    // BUFFERS lines look like `Buffers: shared hit=N read=M`.
    assert!(
        joined.contains("Buffers:") || joined.contains("buffers"),
        "BUFFERS option should surface buffer counts: {joined}"
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "_explain_demo" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn explain_with_format_json_returns_parseable_payload() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let plan = Demo::objects()
        .explain_on(
            &pool,
            ExplainOptions {
                format: ExplainFormat::Json,
                ..ExplainOptions::default()
            },
        )
        .await
        .unwrap();
    // FORMAT JSON returns the entire plan as one giant JSON-array
    // string in column 0. (Postgres collapses the multi-line plan
    // into a single payload row when the format is JSON.)
    let joined = plan.join("\n");
    let parsed: serde_json::Value =
        serde_json::from_str(&joined).expect("EXPLAIN(FORMAT JSON) output should parse");
    assert!(
        parsed.is_array(),
        "EXPLAIN JSON output should be a top-level array"
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "_explain_demo" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}
