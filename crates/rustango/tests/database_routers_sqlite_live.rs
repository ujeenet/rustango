#![cfg(feature = "sqlite")]
//! Live SQLite test for `DATABASE_ROUTERS` + `QuerySet::routed()`
//! (issue #401, building on the #332 registry).
//!
//! Registers a router that sends one model's *reads* to a replica and
//! its *writes* to the primary, then asserts `.routed().fetch()` lands
//! on the replica and `write_pool_for` lands on the primary.
//!
//! Isolation: globally-unique aliases (`r401_*`) + the router only
//! matches this test's table, and we never call `databases::clear()`, so
//! the shared registry isn't disturbed for other tests. Routers are only
//! consulted by `.routed()` / `*_pool_for`, which only this test calls.

use rustango::core::{Model as _, ModelSchema};
use rustango::databases::{self, DatabaseRouter};
use rustango::sql::{sqlx, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "r401_widget")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

async fn pool_with(name: &str) -> Pool {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query("CREATE TABLE r401_widget (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO r401_widget (id, name) VALUES (1, ?)")
        .bind(name)
        .execute(&p)
        .await
        .unwrap();
    p.into()
}

/// Reads of `r401_widget` go to the replica; its writes to the primary.
/// Every other model is deferred (`None`).
struct WidgetReplicaRouter;
impl DatabaseRouter for WidgetReplicaRouter {
    fn db_for_read(&self, model: &ModelSchema) -> Option<String> {
        (model.table == "r401_widget").then(|| "r401_replica".to_owned())
    }
    fn db_for_write(&self, model: &ModelSchema) -> Option<String> {
        (model.table == "r401_widget").then(|| "r401_primary".to_owned())
    }
}

#[tokio::test]
async fn routed_reads_hit_the_replica_writes_hit_the_primary() {
    databases::register("r401_primary", pool_with("primary_row").await);
    databases::register("r401_replica", pool_with("replica_row").await);
    databases::clear_routers();
    databases::register_router(WidgetReplicaRouter);

    // The router decisions.
    assert_eq!(
        databases::route_read(Widget::SCHEMA).as_deref(),
        Some("r401_replica")
    );
    assert_eq!(
        databases::route_write(Widget::SCHEMA).as_deref(),
        Some("r401_primary")
    );

    // `.routed()` sends the read to the replica.
    let read: Vec<String> = Widget::objects()
        .routed()
        .fetch()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(read, vec!["replica_row"], "routed read hits the replica");

    // `write_pool_for` resolves the primary — confirm by reading it back.
    let on_primary: Vec<String> = Widget::objects()
        .fetch_pool(&databases::write_pool_for(Widget::SCHEMA))
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(on_primary, vec!["primary_row"], "write pool is the primary");

    // routed count/exists also run on the replica.
    assert_eq!(Widget::objects().routed().count().await.unwrap(), 1);
    assert!(Widget::objects().routed().exists().await.unwrap());

    // The router defers on tables it doesn't know — falls through to the
    // `"default"` alias (here: no opinion → None).
    databases::clear_routers();
    assert_eq!(
        databases::route_read(Widget::SCHEMA),
        None,
        "no router → defer"
    );
}
