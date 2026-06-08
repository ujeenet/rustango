//! Emission tests for derived-table joins — `join_sub` / `left_join_sub`
//! / `join_lateral` / `left_join_lateral` (Eloquent joinSub / joinLateral,
//! issue #828). `join_sub` is portable (a `JOIN (subquery) AS alias`);
//! `LATERAL` is PG / MySQL only and errors cleanly on SQLite.

use rustango::core::joins::aliased;
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
