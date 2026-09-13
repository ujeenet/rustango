#![cfg(feature = "postgres")]
//! Live PostgreSQL test for `QuerySet::join_lateral` — a correlated
//! `JOIN LATERAL (...)` where the subquery references the outer table
//! (issue #828). Demonstrates the "top-N per group" shape: a LIMIT-1
//! lateral subquery correlated to each customer.
//!
//! Skips silently when `DATABASE_URL` is unset (runs in CI's
//! `postgres_test` job).

use std::sync::OnceLock;

use rustango::core::joins::aliased;
use rustango::core::{Model as _, Op, WhereExpr};
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "sjl_customer")]
#[allow(dead_code)]
pub struct Customer {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "sjl_order")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub customer_id: i64,
    pub total: i64,
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.ok()?.into())
}

async fn fresh(pool: &Pool) {
    let pg = pool.as_postgres().unwrap();
    for ddl in [
        r#"DROP TABLE IF EXISTS "sjl_order" CASCADE"#,
        r#"DROP TABLE IF EXISTS "sjl_customer" CASCADE"#,
        r#"CREATE TABLE "sjl_customer" ("id" BIGSERIAL PRIMARY KEY, "name" VARCHAR(80) NOT NULL)"#,
        r#"CREATE TABLE "sjl_order" ("id" BIGSERIAL PRIMARY KEY, "customer_id" BIGINT NOT NULL, "total" BIGINT NOT NULL)"#,
    ] {
        sqlx::query(ddl).execute(pg).await.unwrap();
    }
}

async fn add_customer(pool: &Pool, name: &str) -> i64 {
    let mut c = Customer {
        id: Auto::default(),
        name: name.into(),
    };
    c.save_pool(pool).await.unwrap();
    *c.id.get().unwrap()
}

async fn add_order(pool: &Pool, customer_id: i64, total: i64) {
    let mut o = Order {
        id: Auto::default(),
        customer_id,
        total,
    };
    o.save_pool(pool).await.unwrap();
}

/// INNER JOIN LATERAL a LIMIT-1 correlated subquery → only customers
/// that have at least one order survive (and exactly once, because the
/// lateral yields a single row per customer). Proves the lateral
/// correlation to the outer table compiles and runs on PG.
#[tokio::test]
async fn join_lateral_correlates_to_outer_and_filters() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let alice = add_customer(&pool, "Alice").await;
    add_customer(&pool, "Bob").await; // no orders
    add_order(&pool, alice, 100).await;
    add_order(&pool, alice, 250).await; // 2 orders, but LIMIT 1 → Alice once

    // (SELECT * FROM sjl_order WHERE sjl_order.customer_id = customer.id
    //  ORDER BY total DESC LIMIT 1) — the "latest/top order per customer".
    let latest = Order::objects()
        .where_raw(WhereExpr::ExprCompare {
            lhs: aliased("sjl_order", "customer_id"),
            op: Op::Eq,
            rhs: aliased("sjl_customer", "id"),
        })
        .order_by(&[("total", true)])
        .limit(1)
        .compile()
        .unwrap();

    let rows = Customer::objects()
        .join_lateral(latest, "lo", WhereExpr::And(vec![]))
        .fetch(&pool)
        .await
        .unwrap();

    // Alice has orders (appears once thanks to LIMIT 1); Bob is dropped
    // by the INNER lateral join.
    assert_eq!(
        rows.len(),
        1,
        "rows: {:?}",
        rows.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert_eq!(rows[0].name, "Alice");
}

/// LEFT JOIN LATERAL preserves every outer row even when the lateral
/// subquery yields none.
#[tokio::test]
async fn left_join_lateral_keeps_all_customers() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let alice = add_customer(&pool, "Alice").await;
    add_customer(&pool, "Bob").await;
    add_order(&pool, alice, 100).await;

    let latest = Order::objects()
        .where_raw(WhereExpr::ExprCompare {
            lhs: aliased("sjl_order", "customer_id"),
            op: Op::Eq,
            rhs: aliased("sjl_customer", "id"),
        })
        .limit(1)
        .compile()
        .unwrap();

    let rows = Customer::objects()
        .left_join_lateral(latest, "lo", WhereExpr::And(vec![]))
        .fetch(&pool)
        .await
        .unwrap();

    let mut names: Vec<String> = rows.into_iter().map(|c| c.name).collect();
    names.sort();
    assert_eq!(names, vec!["Alice", "Bob"]);
}
