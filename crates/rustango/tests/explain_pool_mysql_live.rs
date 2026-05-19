#![cfg(feature = "mysql")]
//! Live MySQL regression for `crate::sql::explain_pool` — closes #272 / T1.10.
//!
//! Mirrors `explain_pool_sqlite_live.rs` + `explain_pool_live.rs`. Uses
//! `EXPLAIN FORMAT=TREE` for text output (8.0.16+) and `EXPLAIN
//! FORMAT=JSON` for JSON. `EXPLAIN ANALYZE` (8.0.18+) wins over either
//! when `analyze = true`.
//!
//! Reads `MYSQL_TEST_URL`. Tests skip silently when unset so
//! `cargo test` stays green offline.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{explain_pool, sqlx, Auto, ExplainFormat, ExplainOptions, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "explain_mysql_demo")]
#[rustango(app = "explain_pool_mysql_live")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
}

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn mysql_pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let mp = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()?;
    Some(Pool::Mysql(mp))
}

async fn fresh(pool: &Pool) {
    let Pool::Mysql(my) = pool else {
        unreachable!()
    };
    sqlx::query("DROP TABLE IF EXISTS `explain_mysql_demo`")
        .execute(my)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE `explain_mysql_demo` (
            `id`    BIGINT       NOT NULL AUTO_INCREMENT PRIMARY KEY,
            `label` VARCHAR(64)  NOT NULL
        )",
    )
    .execute(my)
    .await
    .unwrap();
    for label in ["alpha", "beta", "gamma"] {
        sqlx::query("INSERT INTO `explain_mysql_demo` (`label`) VALUES (?)")
            .bind(label)
            .execute(my)
            .await
            .unwrap();
    }
}

fn select_query() -> rustango::core::SelectQuery {
    Demo::objects()
        .where_(Demo::label.eq("alpha"))
        .compile()
        .expect("compile")
}

#[tokio::test]
async fn explain_pool_returns_plan_text_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        eprintln!("skipping: MYSQL_TEST_URL not set");
        return;
    };
    fresh(&pool).await;

    let q = select_query();
    let plan = explain_pool(&pool, &q, ExplainOptions::default())
        .await
        .expect("explain text");
    assert!(!plan.is_empty(), "expected non-empty plan");
    // MySQL FORMAT=TREE output references the source table.
    assert!(
        plan.to_ascii_lowercase().contains("explain_mysql_demo"),
        "plan should reference our table, got:\n{plan}"
    );
}

#[tokio::test]
async fn explain_pool_returns_plan_json_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        eprintln!("skipping: MYSQL_TEST_URL not set");
        return;
    };
    fresh(&pool).await;

    let q = select_query();
    let plan = explain_pool(
        &pool,
        &q,
        ExplainOptions {
            format: ExplainFormat::Json,
            ..Default::default()
        },
    )
    .await
    .expect("explain json");
    assert!(!plan.is_empty(), "expected non-empty plan");
    let parsed: serde_json::Value =
        serde_json::from_str(&plan).expect("EXPLAIN FORMAT=JSON should parse");
    // MySQL JSON plan is an object with a `query_block` key.
    assert!(
        parsed.is_object(),
        "expected JSON object from MySQL EXPLAIN, got: {parsed}"
    );
}
