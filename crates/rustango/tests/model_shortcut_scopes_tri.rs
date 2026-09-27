//! Every `Model::*` static shortcut applies the model's global scopes,
//! the same as its `QuerySet` twin (#1675).
//!
//! Seed: three in-scope rows (amounts 3, 5, 7) and two out-of-scope
//! rows (amounts 1, 60), which hold the min and the max.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{Filter, Op, SqlValue, WhereExpr};
use rustango::sql::{Auto, CounterPool as _, Pool};
use rustango::{tri_dialect_test, Model};

fn visible_only() -> WhereExpr {
    WhereExpr::Predicate(Filter::new("visible", Op::Eq, SqlValue::Bool(true)))
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "scope1675_ledger",
    app = "scope1675",
    global_scope(name = "visible", apply = visible_only)
)]
#[allow(dead_code)]
pub struct Ledger {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub tag: String,
    pub amount: i64,
    pub visible: bool,
}

/// Rebuild the table; returns the PKs of the two hidden rows.
async fn seed(pool: &Pool) -> Vec<i64> {
    rustango::testkit::matrix::fresh_table::<Ledger>(pool).await;
    let mut hidden = Vec::new();
    for (tag, amount, visible) in [
        ("a", 3_i64, true),
        ("h", 1, false),
        ("b", 5, true),
        ("h", 60, false),
        ("c", 7, true),
    ] {
        let mut row = Ledger {
            id: Auto::default(),
            tag: tag.into(),
            amount,
            visible,
        };
        row.insert_pool(pool).await.expect("seed row");
        if !visible {
            hidden.push(*row.id.get().expect("pk"));
        }
    }
    hidden
}

async fn noop(_: &Pool) {}

async fn total_rows(pool: &Pool) -> i64 {
    Ledger::objects()
        .without_global_scopes()
        .count(pool)
        .await
        .expect("unscoped count")
}

async fn aggregates_apply_the_scope(pool: &Pool) {
    seed(pool).await;
    let qs_sum: Option<i64> = Ledger::objects().sum("amount", pool).await.unwrap();
    assert_eq!(qs_sum, Some(15));
    assert_eq!(Ledger::sum::<i64>("amount", pool).await.unwrap(), Some(15));
    assert_eq!(Ledger::min::<i64>("amount", pool).await.unwrap(), Some(3));
    assert_eq!(Ledger::max::<i64>("amount", pool).await.unwrap(), Some(7));
    let avg: Option<f64> = Ledger::objects().avg("amount", pool).await.unwrap();
    assert_eq!(Ledger::avg::<f64>("amount", pool).await.unwrap(), avg);
}

async fn destroy_applies_the_scope(pool: &Pool) {
    let hidden = seed(pool).await;
    let visible = *Ledger::first(pool)
        .await
        .unwrap()
        .expect("row")
        .id
        .get()
        .unwrap();
    assert_eq!(Ledger::destroy(hidden, pool).await.unwrap(), 0);
    assert_eq!(Ledger::destroy([visible], pool).await.unwrap(), 1);
    assert_eq!(total_rows(pool).await, 4);
}

async fn delete_where_applies_the_scope(pool: &Pool) {
    seed(pool).await;
    assert_eq!(Ledger::delete_where("tag", "h", pool).await.unwrap(), 0);
    assert_eq!(Ledger::delete_where("tag", "a", pool).await.unwrap(), 1);
    assert_eq!(total_rows(pool).await, 4);
}

async fn reads_apply_the_scope(pool: &Pool) {
    let hidden = seed(pool).await;
    assert_eq!(Ledger::count(pool).await.unwrap(), 3);
    assert_eq!(Ledger::all(pool).await.unwrap().len(), 3);
    assert!(Ledger::find(hidden[0], pool).await.unwrap().is_none());
    assert!(!Ledger::contains_pk(hidden[0], pool).await.unwrap());
    assert!(Ledger::where_("tag", "h", pool).await.unwrap().is_empty());
    let amounts: Vec<i64> = Ledger::pluck("amount", pool).await.unwrap();
    assert_eq!(amounts.iter().sum::<i64>(), 15);
}

async fn writes_apply_the_scope(pool: &Pool) {
    seed(pool).await;
    assert_eq!(
        Ledger::update_where("tag", "h", "amount", 0_i64, pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(Ledger::update_all("amount", 1_i64, pool).await.unwrap(), 3);
    assert_eq!(Ledger::increment_each("amount", 1, pool).await.unwrap(), 3);
    let unscoped: Option<i64> = Ledger::objects()
        .without_global_scopes()
        .sum("amount", pool)
        .await
        .unwrap();
    assert_eq!(unscoped, Some(2 * 3 + 1 + 60));
}

tri_dialect_test! {
    setup: noop,
    scenarios: [
        aggregates_apply_the_scope,
        destroy_applies_the_scope,
        delete_where_applies_the_scope,
        reads_apply_the_scope,
        writes_apply_the_scope,
    ],
}
