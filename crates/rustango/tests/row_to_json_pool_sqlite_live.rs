//! Live regression for v0.36 slice 2 — `select_rows_as_json` +
//! `select_one_row_as_json` against SQLite. Proves the
//! tri-dialect JSON fetch path admin (slice 4) will consume.

#![cfg(feature = "sqlite")]

use rustango::core::{Filter, Model as _, Op, SelectQuery, SqlValue, WhereExpr};
use rustango::sql::{select_one_row_as_json, select_rows_as_json, sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "json_widget")]
#[rustango(app = "json_pool_live")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    pub count: i32,
    pub active: bool,
    pub weight: f64,
}

async fn sqlite_pool_with_widgets() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    sqlx::query(
        r#"CREATE TABLE json_widget (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            name    TEXT NOT NULL,
            count   INTEGER NOT NULL,
            active  INTEGER NOT NULL,
            weight  REAL NOT NULL
        )"#,
    )
    .execute(&pool)
    .await
    .expect("create widgets");
    Pool::Sqlite(pool)
}

fn fields() -> Vec<&'static rustango::core::FieldSchema> {
    Widget::SCHEMA.scalar_fields().collect()
}

#[tokio::test]
async fn select_rows_as_json_pool_decodes_sqlite_rows() {
    let pool = sqlite_pool_with_widgets().await;

    // Seed via ORM (proves the round-trip from insert path → JSON
    // fetch path is symmetric on sqlite).
    let mut a = Widget {
        id: Auto::default(),
        name: "alpha".into(),
        count: 10,
        active: true,
        weight: 1.5,
    };
    a.insert_pool(&pool).await.expect("insert alpha");
    let mut b = Widget {
        id: Auto::default(),
        name: "beta".into(),
        count: 20,
        active: false,
        weight: 2.5,
    };
    b.insert_pool(&pool).await.expect("insert beta");

    let fields = fields();
    let rows = select_rows_as_json(
        &pool,
        &SelectQuery {
            model: Widget::SCHEMA,
            where_clause: WhereExpr::And(Vec::new()),
            search: None,
            joins: Vec::new(),
            order_by: vec![rustango::core::OrderItem::column("id", false)],
            limit: None,
            offset: None,
        },
        &fields,
    )
    .await
    .expect("select_rows_as_json");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "alpha");
    assert_eq!(rows[0]["count"], 10);
    assert_eq!(rows[0]["active"], true);
    assert_eq!(rows[0]["weight"], 1.5);
    assert_eq!(rows[1]["name"], "beta");
    assert_eq!(rows[1]["count"], 20);
    assert_eq!(rows[1]["active"], false);
}

#[tokio::test]
async fn select_one_row_as_json_pool_returns_none_for_miss() {
    let pool = sqlite_pool_with_widgets().await;

    let fields = fields();
    let got = select_one_row_as_json(
        &pool,
        &SelectQuery {
            model: Widget::SCHEMA,
            where_clause: WhereExpr::Predicate(Filter {
                column: "id",
                op: Op::Eq,
                value: SqlValue::I64(9999),
            }),
            search: None,
            joins: Vec::new(),
            order_by: Vec::new(),
            limit: Some(1),
            offset: None,
        },
        &fields,
    )
    .await
    .expect("select_one_row_as_json");
    assert!(got.is_none());
}

#[tokio::test]
async fn select_one_row_as_json_pool_returns_decoded_row_for_hit() {
    let pool = sqlite_pool_with_widgets().await;

    let mut w = Widget {
        id: Auto::default(),
        name: "gamma".into(),
        count: 42,
        active: true,
        weight: 3.14,
    };
    w.insert_pool(&pool).await.expect("insert gamma");
    let pk = w.id.get().copied().expect("gamma pk");

    let fields = fields();
    let got = select_one_row_as_json(
        &pool,
        &SelectQuery {
            model: Widget::SCHEMA,
            where_clause: WhereExpr::Predicate(Filter {
                column: "id",
                op: Op::Eq,
                value: SqlValue::I64(pk),
            }),
            search: None,
            joins: Vec::new(),
            order_by: Vec::new(),
            limit: Some(1),
            offset: None,
        },
        &fields,
    )
    .await
    .expect("select_one_row_as_json")
    .expect("row present");
    assert_eq!(got["name"], "gamma");
    assert_eq!(got["count"], 42);
    assert_eq!(got["active"], true);
    assert_eq!(got["weight"], 3.14);
}
