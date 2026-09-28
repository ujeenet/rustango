//! A NULL cell decodes as JSON `null` on every backend (#1766).
//!
//! sqlx-sqlite decodes NULL into `0` / `false` / `""` without erroring,
//! so SQLite returned `0` for an unset FK while PG and MySQL returned `null`.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{Model as _, SelectQuery, SqlValue};
use rustango::sql::{select_rows_as_json, Auto, ForeignKey, Pool};
use rustango::{tri_dialect_test, Model};
use serde_json::Value;

#[derive(Model, Debug, Clone)]
#[rustango(table = "nulls1766_picker", app = "nulls1766", display = "name")]
#[allow(dead_code)]
pub struct Picker {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "nulls1766_order", app = "nulls1766")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub assigned_picker_id: Option<ForeignKey<Picker, i64>>,
    pub quantity: Option<i64>,
    pub rush: Option<bool>,
    #[rustango(max_length = 32)]
    pub note: Option<String>,
}

const NULLABLE: [&str; 4] = ["assigned_picker_id", "quantity", "rush", "note"];

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Order::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Picker>(pool).await;
    rustango::testkit::matrix::fresh_table::<Order>(pool).await;
}

/// Inserts an all-NULL order and a fully set one, in that order.
async fn seed(pool: &Pool) {
    let mut picker = Picker {
        id: Auto::default(),
        name: "p".into(),
    };
    picker.insert_pool(pool).await.expect("insert picker");
    let picker_pk = picker.id.get().copied().expect("picker pk");

    let mut empty = Order {
        id: Auto::default(),
        assigned_picker_id: None,
        quantity: None,
        rush: None,
        note: None,
    };
    empty.insert_pool(pool).await.expect("insert empty order");
    let mut full = Order {
        id: Auto::default(),
        assigned_picker_id: Some(ForeignKey::unloaded(picker_pk)),
        quantity: Some(7),
        rush: Some(true),
        note: Some("n".into()),
    };
    full.insert_pool(pool).await.expect("insert full order");
}

async fn json_rows(pool: &Pool) -> Vec<Value> {
    let fields: Vec<_> = Order::SCHEMA.scalar_fields().collect();
    let mut q = SelectQuery::new(Order::SCHEMA);
    q.order_by = vec![rustango::core::OrderItem::column("id", false)];
    select_rows_as_json(pool, &q, &fields)
        .await
        .expect("select_rows_as_json")
}

async fn null_cells_decode_as_json_null(pool: &Pool) {
    seed(pool).await;
    let rows = json_rows(pool).await;
    for key in NULLABLE {
        assert_eq!(
            rows[0][key],
            Value::Null,
            "{key} on {}: {:?}",
            pool.dialect().name(),
            rows[0]
        );
    }
}

/// The NULL check must not swallow set values.
async fn set_cells_still_decode(pool: &Pool) {
    seed(pool).await;
    let rows = json_rows(pool).await;
    assert!(rows[1]["assigned_picker_id"].is_i64(), "{:?}", rows[1]);
    assert_eq!(rows[1]["quantity"], 7);
    assert_eq!(rows[1]["rush"], true);
    assert_eq!(rows[1]["note"], "n");
}

/// `values_dict` has its own SQLite cell decoder with the same trap.
async fn values_dict_returns_null_for_null_cells(pool: &Pool) {
    seed(pool).await;
    let rows = Order::objects()
        .order_by(&[("id", false)])
        .values_dict(&NULLABLE)
        .fetch(pool)
        .await
        .expect("values_dict");
    for key in NULLABLE {
        assert_eq!(
            rows[0][key],
            SqlValue::Null,
            "{key} on {}: {:?}",
            pool.dialect().name(),
            rows[0]
        );
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        null_cells_decode_as_json_null,
        set_cells_still_decode,
        values_dict_returns_null_for_null_cells,
    ],
}
