#![cfg(feature = "sqlite")]
//! Live SQLite test for the multi-database registry + `QuerySet::using`
//! (issues #332 / #400). Registers two separate SQLite databases under
//! aliases, seeds them with *different* rows, and asserts that
//! `.using(alias)` routes each read to the right one.
//!
//! Uses globally-unique alias names (the registry is process-wide) and
//! never calls `databases::clear()`, so it stays independent of any
//! other test touching the registry.

use rustango::databases;
use rustango::sql::{sqlx, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "md_widget")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

/// A single-connection SQLite pool seeded with one `md_widget` row per
/// name. `max_connections(1)` keeps the seed + later reads on the same
/// in-memory database.
async fn pool_with(names: &[&str]) -> Pool {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query("CREATE TABLE md_widget (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .execute(&p)
        .await
        .unwrap();
    for (i, name) in names.iter().enumerate() {
        sqlx::query("INSERT INTO md_widget (id, name) VALUES (?, ?)")
            .bind(i64::try_from(i).unwrap() + 1)
            .bind(*name)
            .execute(&p)
            .await
            .unwrap();
    }
    p.into()
}

#[tokio::test]
async fn using_routes_reads_to_the_registered_alias() {
    let primary = pool_with(&["alpha"]).await;
    let replica = pool_with(&["beta", "gamma"]).await;
    databases::register("md332_primary", primary);
    databases::register("md332_replica", replica);

    // Registry lookups.
    assert!(databases::get("md332_primary").is_some());
    assert!(databases::get("md332_missing").is_none());
    assert!(databases::aliases().contains(&"md332_replica".to_owned()));

    // `.using(alias).fetch()` reads from the matching database.
    let from_primary: Vec<String> = Widget::objects()
        .using("md332_primary")
        .fetch()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(from_primary, vec!["alpha"]);

    let from_replica: Vec<String> = Widget::objects()
        .order_by(&[("name", false)])
        .using("md332_replica")
        .fetch()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(from_replica, vec!["beta", "gamma"]);

    // count / exists / first route to the chosen alias too.
    assert_eq!(
        Widget::objects()
            .using("md332_replica")
            .count()
            .await
            .unwrap(),
        2
    );
    assert!(Widget::objects()
        .using("md332_primary")
        .exists()
        .await
        .unwrap());
    let first = Widget::objects()
        .filter("name", "gamma")
        .using("md332_replica")
        .first()
        .await
        .unwrap();
    assert_eq!(first.unwrap().name, "gamma");

    // A row that only exists on the replica is absent from the primary.
    assert!(!Widget::objects()
        .filter("name", "beta")
        .using("md332_primary")
        .exists()
        .await
        .unwrap());
}

#[tokio::test]
#[should_panic(expected = "no database registered under alias")]
async fn using_unknown_alias_panics_with_a_clear_message() {
    // Never registered → loud failure (Django's ConnectionDoesNotExist).
    let _ = Widget::objects().using("md332_definitely_unregistered_alias");
}
