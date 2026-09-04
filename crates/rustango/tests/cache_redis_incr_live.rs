//! `RedisCache::incr` against a live Redis (#1280).
//!
//! The bug: `incr` set the window TTL with `EXPIRE key secs NX`. That
//! flag is **Redis 7.0+** — on 6.x the server answers `ERR wrong number
//! of arguments for 'expire' command`, so `incr` returned `Err` on
//! *every* call. Both callers fail open, so rate limiting
//! (`rate_limit_cache::take` → `Ok((0, 0))`) and account lockout
//! (`account_lockout` → `.unwrap_or(0)`) silently stopped working, while
//! the already-applied `INCRBY` left counters with no TTL at all.
//!
//! Nothing caught it: CI ran no Redis service, and the one existing
//! Redis test never called `incr`.
//!
//! Run this against **both** 6 and 7 — the whole point is version
//! portability:
//!
//! ```bash
//! docker run -d -p 6398:6379 redis:6
//! REDIS_TEST_URL=redis://127.0.0.1:6398/ \
//!   cargo test -p rustango --features cache,cache-redis --test cache_redis_incr_live
//! ```
//!
//! Reads `REDIS_TEST_URL`; skips silently when unset.

#![cfg(feature = "cache-redis")]

use std::time::Duration;

use rustango::cache::redis_backend::RedisCache;
use rustango::cache::Cache;
use tokio::sync::Mutex;

/// Suite-wide lock — the reset below is `FLUSHDB`, which is global, so
/// concurrent tests would wipe each other's counters mid-run.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn cache() -> Option<RedisCache> {
    let url = std::env::var("REDIS_TEST_URL").ok()?;
    Some(RedisCache::new(&url).await.expect("connect REDIS_TEST_URL"))
}

/// The headline regression: `incr` must simply *work*, on every
/// supported Redis. Against 6.x the old `EXPIRE … NX` form made this
/// return `Err` every single time.
#[tokio::test]
async fn incr_with_ttl_succeeds_and_counts() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    let ttl = Some(Duration::from_secs(60));
    assert_eq!(
        redis
            .incr("hits", 1, ttl)
            .await
            .expect("incr must not error"),
        1
    );
    assert_eq!(redis.incr("hits", 1, ttl).await.unwrap(), 2);
    assert_eq!(redis.incr("hits", 5, ttl).await.unwrap(), 7);
}

/// The semantic the `NX` flag was there to provide, kept intact by the
/// Lua form: the TTL is set when the counter is created and **not**
/// pushed forward by later increments. Without that a fixed-window rate
/// limiter slides its window on every request and never reaches the cap.
///
/// Timed rather than introspected because `Cache` exposes no `TTL`
/// (the crate has no `redis` dev-dependency to peek with). Margins are
/// ~0.4s either side of a 2s window.
#[tokio::test]
async fn window_does_not_slide_on_later_increments() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    let ttl = Some(Duration::from_secs(2));
    assert_eq!(redis.incr("win", 1, ttl).await.unwrap(), 1);

    // Mid-window bump. If this reset the TTL, expiry moves to ~t+3.0s.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(redis.incr("win", 1, ttl).await.unwrap(), 2);

    // Past the ORIGINAL window (t+2.0s) but before a slid one (t+3.0s).
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(
        redis.incr("win", 1, ttl).await.unwrap(),
        1,
        "counter should have expired with the original window; a value \
         of 3 means the TTL slid forward and the window never closes"
    );
}

/// `ttl = None` means "no expiry" — the counter must persist rather
/// than inherit some accidental TTL from the script.
#[tokio::test]
async fn incr_without_ttl_leaves_the_counter_persistent() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    assert_eq!(redis.incr("forever", 1, None).await.unwrap(), 1);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        redis.incr("forever", 1, None).await.unwrap(),
        2,
        "a None ttl must not expire the counter"
    );
}

/// `decr` routes through `incr` with a negated step, so it rides the
/// same script — check the negative path compiles down correctly.
#[tokio::test]
async fn decr_goes_negative_through_the_same_script() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    assert_eq!(redis.incr("bal", 5, None).await.unwrap(), 5);
    assert_eq!(redis.decr("bal", 8, None).await.unwrap(), -3);
}
