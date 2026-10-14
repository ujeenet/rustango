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
    if std::env::var_os("RUSTANGO_DB_TEST_BEFORE_ACQUIRE").is_some() {
        eprintln!("RUSTANGO_DB_TEST_BEFORE_ACQUIRE is set; skipping");
        return;
    }
    let (ping, idle, life) = ping_and_bounds(pool);
    assert!(!ping, "a default pool must not ping before every checkout");
    assert!(idle && life, "stale connections must still be recycled");
}

tri_dialect_test! {
    setup: no_setup,
    scenarios: [default_pool_skips_the_acquire_ping],
}
