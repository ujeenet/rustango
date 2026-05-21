//! Django-parity #410 — `m2m_changed` signal fires from
//! `M2MManager::{add_pool, remove_pool, set_pool, clear_pool}`
//! against a live SQLite junction table.

#![cfg(all(feature = "sqlite", feature = "signals"))]

use std::sync::Arc;

use rustango::core::SqlValue;
use rustango::signals::m2m::{
    clear_all, connect_m2m_changed, receiver_count, M2mAction, M2mChangedContext,
};
use rustango::sql::{sqlx, M2MManager, Pool};
use tokio::sync::Mutex;

/// Suite-wide lock — `clear_all` mutates a global registry, races
/// any other test with a receiver connected.
fn suite_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool_with_junction() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    sqlx::query(
        r#"CREATE TABLE post_tags (
            post_id INTEGER NOT NULL,
            tag_id  INTEGER NOT NULL,
            UNIQUE(post_id, tag_id)
        )"#,
    )
    .execute(&pool)
    .await
    .expect("create junction");
    Pool::Sqlite(pool)
}

fn mgr(post_id: i64) -> M2MManager {
    M2MManager {
        src_pk: SqlValue::I64(post_id),
        through: "post_tags",
        src_col: "post_id",
        dst_col: "tag_id",
    }
}

#[tokio::test]
async fn add_fires_with_add_action_and_single_dst_pk() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<M2mChangedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_m2m_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let pool = pool_with_junction().await;
    mgr(1).add_pool(7, &pool).await.unwrap();

    let got = captured.lock().await;
    assert_eq!(got.len(), 1, "exactly one fire, got: {got:?}");
    assert!(matches!(got[0].action, M2mAction::Add));
    assert_eq!(got[0].through, "post_tags");
    assert_eq!(got[0].src_col, "post_id");
    assert_eq!(got[0].dst_col, "tag_id");
    assert_eq!(got[0].src_pk, 1);
    assert_eq!(got[0].dst_pks, vec![7]);
}

#[tokio::test]
async fn remove_fires_with_remove_action() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<M2mChangedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_m2m_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let pool = pool_with_junction().await;
    let m = mgr(1);
    m.add_pool(7, &pool).await.unwrap();
    captured.lock().await.clear(); // ignore the Add for clarity

    m.remove_pool(7, &pool).await.unwrap();

    let got = captured.lock().await;
    assert_eq!(got.len(), 1);
    assert!(matches!(got[0].action, M2mAction::Remove));
    assert_eq!(got[0].dst_pks, vec![7]);
}

#[tokio::test]
async fn set_fires_with_set_action_and_full_new_set() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<M2mChangedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_m2m_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let pool = pool_with_junction().await;
    mgr(1).set_pool(&[7, 8, 9], &pool).await.unwrap();

    let got = captured.lock().await;
    assert_eq!(got.len(), 1, "set fires once, got: {got:?}");
    assert!(matches!(got[0].action, M2mAction::Set));
    assert_eq!(got[0].dst_pks, vec![7, 8, 9]);
}

#[tokio::test]
async fn set_with_empty_slice_fires_set_with_empty_pks() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<M2mChangedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_m2m_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let pool = pool_with_junction().await;
    mgr(1).add_pool(7, &pool).await.unwrap();
    captured.lock().await.clear();

    mgr(1).set_pool(&[], &pool).await.unwrap();

    let got = captured.lock().await;
    assert_eq!(got.len(), 1);
    assert!(matches!(got[0].action, M2mAction::Set));
    assert!(got[0].dst_pks.is_empty());
}

#[tokio::test]
async fn clear_fires_with_clear_action_and_empty_pks() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<M2mChangedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_m2m_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let pool = pool_with_junction().await;
    let m = mgr(1);
    m.add_pool(7, &pool).await.unwrap();
    m.add_pool(8, &pool).await.unwrap();
    captured.lock().await.clear();

    m.clear_pool(&pool).await.unwrap();

    let got = captured.lock().await;
    assert_eq!(got.len(), 1);
    assert!(matches!(got[0].action, M2mAction::Clear));
    assert!(got[0].dst_pks.is_empty());
}

#[tokio::test]
async fn no_receivers_does_not_break_m2m_ops() {
    let _g = suite_lock().lock().await;
    clear_all();
    assert_eq!(receiver_count(), 0);

    let pool = pool_with_junction().await;
    let m = mgr(1);
    // All four mutating ops must succeed with no receivers connected.
    m.add_pool(7, &pool).await.unwrap();
    m.add_pool(8, &pool).await.unwrap();
    m.remove_pool(7, &pool).await.unwrap();
    m.set_pool(&[9, 10], &pool).await.unwrap();
    m.clear_pool(&pool).await.unwrap();
}
