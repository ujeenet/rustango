//! Emission tests for derived-table joins — `join_sub` / `left_join_sub`
//! / `join_lateral` / `left_join_lateral` (Eloquent joinSub / joinLateral,
//! issue #828). `join_sub` is portable (a `JOIN (subquery) AS alias`);
//! `LATERAL` is PG / MySQL only and errors cleanly on SQLite.

use rustango::core::joins::aliased;
use rustango::core::window::row_number;
use rustango::core::{Op, WhereExpr};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "sj_customer")]
#[allow(dead_code)]
pub struct Customer {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    name: String,
}

#[derive(Model)]
#[rustango(table = "sj_order")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    id: i64,
    customer_id: i64,
    total: i64,
}

/// `Customer`s that appear in a derived table of orders.
fn customers_with_orders() -> rustango::query::QuerySet<Customer> {
    let sub = Order::objects().compile().unwrap();
    Customer::objects().join_sub(
        sub,
        "o",
        WhereExpr::ExprCompare {
            lhs: aliased("o", "customer_id"),
            op: Op::Eq,
            rhs: aliased("customer", "id"),
        },
    )
}

#[test]
fn join_sub_emits_derived_table_join_on_pg() {
    let q = customers_with_orders().compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.contains(r#"INNER JOIN (SELECT "#)
            && sql.contains(r#"FROM "sj_order") AS "o" ON "o"."customer_id" = "customer"."id""#),
        "pg join_sub: {sql}"
    );
    // Main-table columns get qualified once a join is present.
    assert!(sql.contains(r#""sj_customer"."name""#), "qualified: {sql}");
}

#[test]
fn join_sub_is_portable_to_sqlite_and_mysql() {
    let q = customers_with_orders().compile().unwrap();
    // SQLite — double quotes, no error (derived-table join is portable).
    let sqlite = Sqlite.compile_select(&q).unwrap().sql;
    assert!(
        sqlite.contains(r#"FROM "sj_order") AS "o" ON "o"."customer_id" = "customer"."id""#),
        "sqlite join_sub: {sqlite}"
    );
    // MySQL — backtick quoting.
    let mysql = MySql.compile_select(&q).unwrap().sql;
    assert!(
        mysql.contains("FROM `sj_order`) AS `o` ON `o`.`customer_id` = `customer`.`id`"),
        "mysql join_sub: {mysql}"
    );
}

#[test]
fn left_join_sub_emits_left_join() {
    let sub = Order::objects().compile().unwrap();
    let q = Customer::objects()
        .left_join_sub(
            sub,
            "o",
            WhereExpr::ExprCompare {
                lhs: aliased("o", "customer_id"),
                op: Op::Eq,
                rhs: aliased("customer", "id"),
            },
        )
        .compile()
        .unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.contains(r#"LEFT JOIN (SELECT "#) && sql.contains(r#"FROM "sj_order") AS "o""#),
        "{sql}"
    );
}

#[test]
fn join_lateral_emits_lateral_keyword_with_on_true_on_pg() {
    // Lateral subquery correlated to the outer customer via the
    // subquery's own WHERE (AliasedColumn to the outer table).
    let latest = Order::objects()
        .where_raw(WhereExpr::ExprCompare {
            lhs: aliased("sj_order", "customer_id"),
            op: Op::Eq,
            rhs: aliased("customer", "id"),
        })
        .compile()
        .unwrap();
    let q = Customer::objects()
        .left_join_lateral(latest, "lo", WhereExpr::And(vec![]))
        .compile()
        .unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.contains(r#"LEFT JOIN LATERAL (SELECT"#),
        "lateral kw: {sql}"
    );
    assert!(sql.contains(r#") AS "lo" ON true"#), "on true: {sql}");
}

#[test]
fn lateral_errors_cleanly_on_sqlite() {
    let latest = Order::objects().compile().unwrap();
    let q = Customer::objects()
        .join_lateral(latest, "lo", WhereExpr::And(vec![]))
        .compile()
        .unwrap();
    let err = Sqlite.compile_select(&q).unwrap_err();
    assert!(
        matches!(err, SqlError::LateralJoinNotSupported { dialect: "sqlite" }),
        "expected LateralJoinNotSupported, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// #1035 — a window/aggregate query as a derived-table source. Window
// functions compile to an `AggregateQuery`; `join_sub` now accepts one
// (via `impl Into<DerivedSource>`), so the "filter on a window result"
// idiom (rank ≤ N per group) is expressible as a derived table.
// ---------------------------------------------------------------------------

/// `row_number() OVER (PARTITION BY customer_id ORDER BY total DESC)`,
/// projected alongside the grouped columns so the outer query can join +
/// filter on it.
fn ranked_orders() -> rustango::core::AggregateQuery {
    Order::objects()
        .aggregate()
        .group_by("id")
        .group_by("customer_id")
        .annotate(
            "rn",
            row_number()
                .partition_by("customer_id")
                .order_by(&[("total", true)])
                .into(),
        )
        .compile()
        .unwrap()
}

#[test]
fn join_sub_accepts_window_aggregate_on_pg() {
    let q = Customer::objects()
        .join_sub(
            ranked_orders(),
            "r",
            WhereExpr::ExprCompare {
                lhs: aliased("r", "customer_id"),
                op: Op::Eq,
                rhs: aliased("customer", "id"),
            },
        )
        .compile()
        .unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    // The aggregate/window query is wrapped as a derived table, OVER clause
    // and all, and joined on the projected group column.
    assert!(
        sql.contains(r#"INNER JOIN (SELECT "#),
        "derived table: {sql}"
    );
    assert!(sql.contains("OVER ("), "window OVER inside subquery: {sql}");
    assert!(
        sql.contains(r#") AS "r" ON "r"."customer_id" = "customer"."id""#),
        "alias + ON: {sql}"
    );
}

#[test]
fn window_aggregate_derived_table_is_tri_dialect() {
    let q = Customer::objects()
        .join_sub(
            ranked_orders(),
            "r",
            WhereExpr::ExprCompare {
                lhs: aliased("r", "customer_id"),
                op: Op::Eq,
                rhs: aliased("customer", "id"),
            },
        )
        .compile()
        .unwrap();
    // Window functions are supported on all three backends, so a
    // (non-lateral) windowed derived table emits cleanly everywhere.
    for (name, sql) in [
        ("sqlite", Sqlite.compile_select(&q).unwrap().sql),
        ("mysql", MySql.compile_select(&q).unwrap().sql),
    ] {
        assert!(sql.contains("OVER ("), "{name}: window missing: {sql}");
        assert!(
            sql.to_lowercase().contains("inner join (select"),
            "{name}: derived table missing: {sql}"
        );
    }
}

#[test]
fn join_lateral_accepts_window_aggregate_on_pg() {
    // The window/aggregate derived table also composes with LATERAL
    // (PG / MySQL). Emits `JOIN LATERAL (SELECT … OVER … ) AS r ON true`.
    let q = Customer::objects()
        .join_lateral(ranked_orders(), "r", WhereExpr::And(vec![]))
        .compile()
        .unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.contains("INNER JOIN LATERAL (SELECT "),
        "lateral kw: {sql}"
    );
    assert!(sql.contains("OVER ("), "window inside lateral: {sql}");
    assert!(sql.contains(r#") AS "r" ON true"#), "on true: {sql}");
}

#[test]
fn join_sub_does_not_project_derived_columns() {
    // The SELECT list is the base model's columns only — a derived-table
    // join is for filtering/relating, so a typed fetch still decodes
    // `Customer`. (No `o.*` / `o__col` aliases in the projection.)
    let q = customers_with_orders().compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    let select_list = &sql[..sql.find(" FROM ").unwrap()];
    assert!(
        !select_list.contains("\"o\""),
        "derived cols leaked: {select_list}"
    );
}
