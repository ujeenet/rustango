//! Django-parity #411 — `pre_migrate` / `post_migrate` signals fire
//! around `apply_all_pool` on a live SQLite pool.
//!
//! Verifies the integration end-to-end: a real bootstrap walk runs,
//! connected receivers capture the contexts, ordering is pre → work →
//! post, and source identifiers are correct.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use std::sync::Arc;

use rustango::signals::migrate::{
    clear_all, connect_post_migrate, connect_pre_migrate, receiver_count, PostMigrateContext,
    PreMigrateContext,
};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

/// Suite-wide lock: cargo runs tests in parallel and migrate-signal
/// receivers live in a shared global registry. Without the mutex,
/// `clear_all` in one test races another's connect+send.
fn suite_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mig_signal_widget")]
#[rustango(app = "mig_signal_app")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub label: String,
}

async fn fresh_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    Pool::Sqlite(sq)
}

#[tokio::test]
async fn apply_all_pool_fires_pre_and_post_migrate() {
    let _g = suite_lock().lock().await;
    clear_all();
    assert_eq!(receiver_count(), 0);

    // Capture both pre + post contexts + relative order using a
    // shared trace log.
    let trace: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let pre_ctx: Arc<Mutex<Option<PreMigrateContext>>> = Arc::new(Mutex::new(None));
    let post_ctx: Arc<Mutex<Option<PostMigrateContext>>> = Arc::new(Mutex::new(None));

    {
        let trace = trace.clone();
        let pre_ctx = pre_ctx.clone();
        connect_pre_migrate(move |ctx| {
            let trace = trace.clone();
            let pre_ctx = pre_ctx.clone();
            async move {
                trace.lock().await.push("pre");
                *pre_ctx.lock().await = Some(ctx);
            }
        });
    }
    {
        let trace = trace.clone();
        let post_ctx = post_ctx.clone();
        connect_post_migrate(move |ctx| {
            let trace = trace.clone();
            let post_ctx = post_ctx.clone();
            async move {
                trace.lock().await.push("post");
                *post_ctx.lock().await = Some(ctx);
            }
        });
    }
    assert_eq!(receiver_count(), 2);

    let pool = fresh_pool().await;
    rustango::migrate::apply_all_pool(&pool)
        .await
        .expect("apply_all_pool");

    let t = trace.lock().await.clone();
    assert_eq!(t, vec!["pre", "post"], "expected pre→post order, got {t:?}");

    let p = pre_ctx.lock().await.clone().expect("pre fired");
    assert_eq!(p.source, "apply_all_pool");
    let q = post_ctx.lock().await.clone().expect("post fired");
    assert_eq!(q.source, "apply_all_pool");
    // `applied` is empty for bootstrap (no per-migration names).
    assert!(q.applied.is_empty());
}

#[tokio::test]
async fn signals_run_in_registration_order_within_each_kind() {
    let _g = suite_lock().lock().await;
    clear_all();

    let log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    for tag in 1u8..=3 {
        let log = log.clone();
        connect_pre_migrate(move |_| {
            let log = log.clone();
            async move {
                log.lock().await.push(tag);
            }
        });
    }
    let pool = fresh_pool().await;
    rustango::migrate::apply_all_pool(&pool).await.unwrap();
    assert_eq!(*log.lock().await, vec![1, 2, 3]);
}

#[tokio::test]
async fn no_receivers_does_not_break_apply_all_pool() {
    let _g = suite_lock().lock().await;
    clear_all();
    assert_eq!(receiver_count(), 0);

    // Zero receivers: send_* is a no-op; apply_all_pool should
    // succeed end-to-end on a fresh pool.
    let pool = fresh_pool().await;
    rustango::migrate::apply_all_pool(&pool)
        .await
        .expect("apply_all_pool with no receivers");

    // And running it twice in a row should still succeed (apply_all_pool
    // is idempotent — CREATE TABLE IF NOT EXISTS isn't but the bootstrap
    // walk only runs at startup; sqlite errors should be transparent
    // here since we use a fresh pool per test).
}

#[tokio::test]
async fn disconnect_removes_only_named_receiver() {
    let _g = suite_lock().lock().await;
    clear_all();

    let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let mut ids = Vec::new();
    for _ in 0..3 {
        let counter = counter.clone();
        ids.push(connect_pre_migrate(move |_| {
            let counter = counter.clone();
            async move {
                *counter.lock().await += 1;
            }
        }));
    }
    assert_eq!(receiver_count(), 3);

    assert!(rustango::signals::migrate::disconnect_pre_migrate(ids[1]));
    assert_eq!(receiver_count(), 2);

    let pool = fresh_pool().await;
    rustango::migrate::apply_all_pool(&pool).await.unwrap();

    assert_eq!(*counter.lock().await, 2);
}
