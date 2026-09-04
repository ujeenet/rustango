#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::join_sub` — a derived-table join is
//! portable across all three backends (issue #828). Confirms the join
//! filters the base-model rows correctly and a typed fetch still decodes
//! the base model.

use rustango::core::joins::aliased;
use rustango::core::window::row_number;
use rustango::core::{Expr, Op, SqlValue, WhereExpr};
use rustango::sql::{sqlx, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sj_customer")]
#[allow(dead_code)]
pub struct Customer {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "sj_order")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub customer_id: ForeignKey<Customer, i64>,
    pub total: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query(
        "CREATE TABLE sj_customer (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE sj_order (id INTEGER PRIMARY KEY AUTOINCREMENT, customer_id INTEGER NOT NULL, total INTEGER NOT NULL)",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
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
        customer_id: ForeignKey::from(customer_id),
        total,
    };
    o.save_pool(pool).await.unwrap();
}

#[tokio::test]
async fn join_sub_filters_to_customers_with_orders() {
    let pool = make_pool().await;
    let alice = add_customer(&pool, "Alice").await;
    let _bob = add_customer(&pool, "Bob").await; // no orders
    add_order(&pool, alice, 100).await; // exactly one → no duplicate row

    // INNER JOIN (SELECT * FROM sj_order) AS o ON o.customer_id = customer.id
    let sub = Order::objects().compile().unwrap();
    let rows = Customer::objects()
        .join_sub(
            sub,
            "o",
            WhereExpr::ExprCompare {
                lhs: aliased("o", "customer_id"),
                op: Op::Eq,
                rhs: aliased("sj_customer", "id"),
            },
        )
        .fetch(&pool)
        .await
        .unwrap();

    // Only Alice has an order; Bob is excluded by the inner join. The
    // typed fetch decodes the base `Customer` model.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Alice");
}

#[tokio::test]
async fn left_join_sub_keeps_unmatched_rows() {
    let pool = make_pool().await;
    let alice = add_customer(&pool, "Alice").await;
    add_customer(&pool, "Bob").await; // no orders
    add_order(&pool, alice, 100).await;

    // Filter the derived table to a customer that doesn't exist → the
    // LEFT JOIN still returns every customer (no rows dropped).
    let sub = Order::objects().compile().unwrap();
    let rows = Customer::objects()
        .left_join_sub(
            sub,
            "o",
            WhereExpr::ExprCompare {
                lhs: aliased("o", "customer_id"),
                op: Op::Eq,
                rhs: aliased("sj_customer", "id"),
            },
        )
        .fetch(&pool)
        .await
        .unwrap();
    // Alice matches her order; Bob is preserved by the LEFT JOIN.
    let mut names: Vec<String> = rows.into_iter().map(|c| c.name).collect();
    names.sort();
    assert_eq!(names, vec!["Alice", "Bob"]);
}

/// #1035 — "top-N per group" via a window query joined as a derived
/// table. The inner query ranks each customer's orders by `total` DESC
/// with `ROW_NUMBER() OVER (PARTITION BY customer_id ...)`; the outer
/// query joins that ranked derived table back to the base rows and keeps
/// only rank ≤ 2. Django's `annotate(rank=Window(...)).filter(rank__lte=2)`
/// idiom, which previously had no rustango equivalent (windows compile to
/// an `AggregateQuery`, which `join_sub` didn't accept).
#[tokio::test]
async fn window_derived_table_yields_top_n_per_group() {
    let pool = make_pool().await;
    let alice = add_customer(&pool, "Alice").await;
    let bob = add_customer(&pool, "Bob").await;
    // Alice: 100, 80, 40 → top-2 are 100, 80.
    add_order(&pool, alice, 100).await;
    add_order(&pool, alice, 80).await;
    add_order(&pool, alice, 40).await;
    // Bob: 200, 50 → top-2 are both (only two orders).
    add_order(&pool, bob, 200).await;
    add_order(&pool, bob, 50).await;

    // Inner: ROW_NUMBER() per customer, ordered by total DESC, projected
    // alongside the id + total so the outer query can join + filter on it.
    let ranked = Order::objects()
        .aggregate()
        .group_by("id")
        .group_by("customer_id")
        .group_by("total")
        .annotate(
            "rn",
            row_number()
                .partition_by("customer_id")
                .order_by(&[("total", true)]) // DESC
                .into(),
        )
        .compile()
        .unwrap();

    // Outer: join the ranked derived table back to the base orders on id,
    // keeping only the top-2 per customer (rn <= 2).
    let rows = Order::objects()
        .join_sub(
            ranked,
            "r",
            WhereExpr::And(vec![
                WhereExpr::ExprCompare {
                    lhs: aliased("r", "id"),
                    op: Op::Eq,
                    rhs: aliased("sj_order", "id"),
                },
                WhereExpr::ExprCompare {
                    lhs: aliased("r", "rn"),
                    op: Op::Lte,
                    rhs: Expr::Literal(SqlValue::I64(2)),
                },
            ]),
        )
        .fetch(&pool)
        .await
        .unwrap();

    // 2 (Alice's top-2) + 2 (all of Bob's) = 4 rows; Alice's 40 is dropped.
    let mut totals: Vec<i64> = rows.iter().map(|o| o.total).collect();
    totals.sort_unstable();
    assert_eq!(totals, vec![50, 80, 100, 200], "top-2 per customer");
}

#[tokio::test]
async fn join_lateral_errors_on_sqlite() {
    let pool = make_pool().await;
    add_customer(&pool, "Alice").await;
    let sub = Order::objects().compile().unwrap();
    let err = Customer::objects()
        .join_lateral(sub, "lo", WhereExpr::And(vec![]))
        .fetch(&pool)
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("LATERAL"),
        "expected a LATERAL-unsupported error on SQLite, got: {err}"
    );
}
