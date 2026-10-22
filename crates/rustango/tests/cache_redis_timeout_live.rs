//! A stalled Redis fails a `RedisCache` call fast instead of hanging it.
//!
//! Reads `REDIS_TEST_URL`; skips silently when unset:
//!
//! ```bash
//! docker run -d -p 6399:6379 redis:7-alpine
//! REDIS_TEST_URL=redis://127.0.0.1:6399/ \
//!   cargo test -p rustango --features cache,cache-redis --test cache_redis_timeout_live
//! ```

#![cfg(feature = "cache-redis")]

use std::time::{Duration, Instant};

use rustango::cache::redis_backend::RedisCache;
use rustango::cache::Cache;

/// `CLIENT PAUSE` stalls every other client, as a frozen server would.
#[tokio::test]
async fn a_paused_server_times_out_the_call() {
    let Ok(url) = std::env::var("REDIS_TEST_URL") else {
        return;
    };
    let cache = RedisCache::new(&url).await.expect("connect");
    cache.set("timeout:k", "v", None).await.expect("warm up");

    let admin = redis::Client::open(url.as_str()).unwrap();
    let mut admin = admin.get_multiplexed_async_connection().await.unwrap();
    redis::cmd("CLIENT")
        .arg("PAUSE")
        .arg(8000)
        .query_async::<()>(&mut admin)
        .await
        .expect("CLIENT PAUSE");

    let start = Instant::now();
    let got = cache.get("timeout:k").await;
    let took = start.elapsed();
    redis::cmd("CLIENT")
        .arg("UNPAUSE")
        .query_async::<()>(&mut admin)
        .await
        .ok();
    assert!(got.is_err(), "answered after {took:?}: {got:?}");
    assert!(took < Duration::from_secs(6), "took {took:?}");
}
