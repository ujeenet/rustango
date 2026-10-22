//! A stalled Redis fails a `RedisCache` call fast instead of hanging it.
//!
//! Talks to a local listener that stops answering, never to a shared
//! server, so other Redis suites can run alongside:
//!
//! ```bash
//! cargo test -p rustango --features cache,cache-redis --test cache_redis_timeout_live
//! ```

#![cfg(feature = "cache-redis")]

use std::time::{Duration, Instant};

use rustango::cache::redis_backend::{RedisCache, RedisTimeouts};
use rustango::cache::Cache;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A Redis address that accepts connections and goes silent, after
/// answering the connect handshake (two `CLIENT SETINFO`) when `handshake`.
async fn stalled_server(handshake: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut sock, _)) = listener.accept().await {
            if handshake {
                let mut buf = [0u8; 4096];
                if sock.read(&mut buf).await.unwrap_or(0) > 0 {
                    let _ = sock.write_all(b"+OK\r\n+OK\r\n").await;
                }
            }
            held.push(sock);
        }
    });
    format!("redis://{addr}/")
}

fn short() -> RedisTimeouts {
    RedisTimeouts::new(Duration::from_millis(500), Duration::from_millis(500))
}

#[tokio::test]
async fn a_stalled_server_times_out_the_call() {
    let url = stalled_server(true).await;
    let cache = RedisCache::with_timeouts(&url, None, short())
        .await
        .expect("connect");
    let start = Instant::now();
    let got = cache.get("timeout:k").await;
    let took = start.elapsed();
    assert!(got.is_err(), "answered after {took:?}: {got:?}");
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[tokio::test]
async fn a_silent_server_times_out_the_connect() {
    let url = stalled_server(false).await;
    let start = Instant::now();
    let got = RedisCache::with_timeouts(&url, None, short()).await;
    let took = start.elapsed();
    assert!(got.is_err(), "connected after {took:?}");
    assert!(took < Duration::from_secs(8), "took {took:?}");
}
