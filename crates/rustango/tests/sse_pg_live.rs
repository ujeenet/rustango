#![cfg(all(feature = "postgres", feature = "sse"))]
//! `PgEventBus` against a live PostgreSQL: keyed delivery across buses,
//! commit-bound publish, schema scoping and resync after a lost connection.

use std::time::Duration;

use rustango::core::SqlValue;
use rustango::sql::Pool;
use rustango::sse::{PgEventBus, Recv, Subscription};

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(Pool::connect(&url).await.expect("connect"))
}

fn channel() -> String {
    format!("t{}", uuid::Uuid::new_v4().simple())
}

async fn next(sub: &mut Subscription<String>) -> Option<Recv<String>> {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .ok()
        .flatten()
}

async fn nothing_within(sub: &mut Subscription<String>, ms: u64) -> bool {
    tokio::time::timeout(Duration::from_millis(ms), sub.recv())
        .await
        .is_err()
}

/// One bus publishes, another on the same channel (as another process
/// would) delivers to that key's subscribers only.
#[tokio::test]
async fn an_event_reaches_the_keys_subscribers_on_every_bus() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    let ch = channel();
    let web: PgEventBus<i64, String> = PgEventBus::connect(&pool, &ch, 16).await.unwrap();
    let worker: PgEventBus<i64, String> = PgEventBus::connect(&pool, &ch, 16).await.unwrap();
    let (mut alice, mut bob) = (web.subscribe(1), web.subscribe(2));
    let mut own = worker.subscribe(1);
    worker.publish(&1, &"hello".to_owned()).await.unwrap();
    assert_eq!(next(&mut alice).await, Some(Recv::Event("hello".into())));
    assert_eq!(next(&mut own).await, Some(Recv::Event("hello".into())));
    assert!(nothing_within(&mut bob, 300).await, "another key got it");
}

/// Inside `atomic` the event goes out at the commit, never on a rollback.
#[tokio::test]
async fn a_publish_in_a_block_waits_for_the_commit() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    let bus: PgEventBus<i64, String> = PgEventBus::connect(&pool, &channel(), 16).await.unwrap();
    let mut sub = bus.subscribe(1);
    let b = bus.clone();
    let rolled: Result<(), _> = rustango::sql::atomic(&pool, move |_tx| {
        Box::pin(async move {
            b.publish(&1, &"rolled back".to_owned()).await.unwrap();
            Err(rustango::sql::ExecError::AtomicAborted)
        })
    })
    .await;
    assert!(rolled.is_err());
    let b = bus.clone();
    rustango::sql::atomic(&pool, move |_tx| {
        Box::pin(async move {
            b.publish(&1, &"committed".to_owned()).await.unwrap();
            Ok(())
        })
    })
    .await
    .unwrap();
    assert_eq!(next(&mut sub).await, Some(Recv::Event("committed".into())));
}

/// A pool scoped to another schema does not hear the event.
#[tokio::test]
async fn another_schemas_bus_does_not_hear_it() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    let opts = pool
        .as_postgres()
        .unwrap()
        .connect_options()
        .as_ref()
        .clone()
        .options([("search_path", "tenant_b")]);
    let other = Pool::Postgres(sqlx::PgPool::connect_with(opts).await.expect("connect"));
    let ch = channel();
    let a: PgEventBus<i64, String> = PgEventBus::connect(&pool, &ch, 16).await.unwrap();
    let b: PgEventBus<i64, String> = PgEventBus::connect(&other, &ch, 16).await.unwrap();
    let (mut on_a, mut on_b) = (a.subscribe(1), b.subscribe(1));
    a.publish(&1, &"for a".to_owned()).await.unwrap();
    assert_eq!(next(&mut on_a).await, Some(Recv::Event("for a".into())));
    assert!(nothing_within(&mut on_b, 300).await, "crossed schemas");
}

/// A lost connection is re-opened and subscribers are told to resync.
#[tokio::test]
async fn a_lost_connection_resyncs_then_delivers() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    let ch = channel();
    let bus: PgEventBus<i64, String> = PgEventBus::connect(&pool, &ch, 16).await.unwrap();
    let mut sub = bus.subscribe(1);
    let killed: Vec<(bool,)> = rustango::sql::raw_query_pool(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE query LIKE '%' || $1 || '%' AND query LIKE 'LISTEN%'",
        vec![SqlValue::String(ch.clone())],
        &pool,
    )
    .await
    .expect("terminate");
    assert_eq!(killed.len(), 1, "no listener connection found");
    assert_eq!(next(&mut sub).await, Some(Recv::Resync));
    bus.publish(&1, &"after".to_owned()).await.unwrap();
    assert_eq!(next(&mut sub).await, Some(Recv::Event("after".into())));
}

/// Over PostgreSQL's NOTIFY limit is refused before the database.
#[tokio::test]
async fn an_oversized_event_is_refused() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    let bus: PgEventBus<i64, String> = PgEventBus::connect(&pool, &channel(), 16).await.unwrap();
    let err = bus.publish(&1, &"x".repeat(9000)).await.unwrap_err();
    assert!(matches!(err, rustango::sse::BusError::TooLarge(_)), "{err}");
}
