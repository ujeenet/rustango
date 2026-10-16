//! #2674 — a pool built with the defaults skips sqlx's per-checkout ping.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::Pool;
use rustango::tri_dialect_test;

async fn no_setup(_pool: &Pool) {}

/// `(test_before_acquire, idle_timeout bounded, max_lifetime bounded)` of the live pool.
fn ping_and_bounds(pool: &Pool) -> (bool, bool, bool) {
    macro_rules! read {
        ($p:expr) => {{
            let o = $p.options();
            (
                o.get_test_before_acquire(),
                o.get_idle_timeout().is_some(),
                o.get_max_lifetime().is_some(),
            )
        }};
    }
    #[cfg(feature = "postgres")]
    if let Some(p) = pool.as_postgres() {
        return read!(p);
    }
    #[cfg(feature = "mysql")]
    if let Some(p) = pool.as_mysql() {
        return read!(p);
    }
    #[cfg(feature = "sqlite")]
    if let Some(p) = pool.as_sqlite() {
        return read!(p);
    }
    unreachable!("a pool is one of the three backends")
}

/// The ping is off and the idle and lifetime bounds stay on.
async fn default_pool_skips_the_acquire_ping(pool: &Pool) {
    assert!(
        std::env::var_os("RUSTANGO_DB_TEST_BEFORE_ACQUIRE").is_none(),
        "unset RUSTANGO_DB_TEST_BEFORE_ACQUIRE: this test checks the default"
    );
    let (ping, idle, life) = ping_and_bounds(pool);
    assert!(!ping, "a default pool must not ping before every checkout");
    assert!(idle && life, "stale connections must still be recycled");
}

/// With the ping off, a connection the server dropped while idle is
/// still checked once it sat idle past the threshold (rel-0608 performance-002).
async fn a_connection_dropped_while_idle_is_replaced(pool: &Pool) {
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    drop_idle_then_query(pool).await;
    #[cfg(not(any(feature = "postgres", feature = "mysql")))]
    let _ = pool;
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn drop_idle_then_query(pool: &Pool) {
    use rustango::sql::raw_execute_pool;
    use rustango::testkit::matrix::Backend;

    let backend = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(_) => Backend::Postgres,
        #[cfg(feature = "mysql")]
        Pool::Mysql(_) => Backend::MySql,
        #[allow(unreachable_patterns)]
        _ => return, // SQLite has no server to drop it.
    };
    // Settings, not env: a set_var here races the SQLite arms' env reads (pr-1408-r2 tests-003).
    // First call wins; every caller in this binary installs the same value.
    let threshold = std::time::Duration::from_secs(3);
    rustango::sql::configure_pools(rustango::sql::PoolTuning {
        idle_ping_after: Some(threshold),
        ..Default::default()
    });
    let fresh = backend.pool().await.expect("the live URL is set");
    let past_threshold = || tokio::time::sleep(threshold + std::time::Duration::from_millis(300));

    // Below the threshold the dead connection is handed out unpinged:
    // the hot path pays no round trip (pr-1408-r2 tests-001).
    kill_idle(&fresh, pool).await;
    let stale = raw_execute_pool(&fresh, "SELECT 1", vec![]).await;
    assert!(stale.is_err(), "a recently idle connection was pinged");

    // Past the threshold, so the next checkout pings whatever the failure left idle.
    past_threshold().await;
    kill_idle(&fresh, pool).await;
    past_threshold().await;
    let alive = raw_execute_pool(&fresh, "SELECT 1", vec![]).await;
    assert!(alive.is_ok(), "the dead idle connection was handed out: {alive:?}");
}

/// Kill, server-side via `pool`, the one connection `fresh` keeps idle,
/// and wait until the server no longer lists it.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn kill_idle(fresh: &Pool, pool: &Pool) {
    use rustango::sql::{sqlx, sqlx::Executor as _};
    let (kill, listed): (i64, String) = match (fresh, pool) {
        #[cfg(feature = "postgres")]
        (Pool::Postgres(f), Pool::Postgres(p)) => {
            let id: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(f)
                .await
                .unwrap();
            let kill = format!("SELECT pg_terminate_backend({id})");
            p.execute(sqlx::raw_sql(&kill)).await.expect("kill");
            (id.into(), format!("SELECT COUNT(*) FROM pg_stat_activity WHERE pid = {id}"))
        }
        #[cfg(feature = "mysql")]
        (Pool::Mysql(f), Pool::Mysql(p)) => {
            let id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
                .fetch_one(f)
                .await
                .unwrap();
            p.execute(sqlx::raw_sql(&format!("KILL {id}"))).await.expect("kill");
            let listed = format!("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID = {id}");
            (i64::try_from(id).unwrap(), listed)
        }
        #[allow(unreachable_patterns)]
        _ => unreachable!("same backend"),
    };
    // KILL returns before the server closes the socket (seen under load).
    for _ in 0..100 {
        let n = match pool {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => sqlx::query_scalar::<_, i64>(&listed).fetch_one(p).await,
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => sqlx::query_scalar::<_, i64>(&listed).fetch_one(p).await,
            #[allow(unreachable_patterns)]
            _ => unreachable!("same backend"),
        };
        if n.expect("list connections") == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("connection {kill} still listed after its kill");
}

tri_dialect_test! {
    setup: no_setup,
    scenarios: [
        default_pool_skips_the_acquire_ping,
        a_connection_dropped_while_idle_is_replaced,
    ],
}
