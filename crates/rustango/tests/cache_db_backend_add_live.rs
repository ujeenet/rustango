//! `DatabaseCache::add` set-if-absent semantics on **Postgres and
//! MySQL** (#1281). The SQLite arm is covered in
//! `cache_db_backend_sqlite_live.rs`.
//!
//! Worth its own file because the two arms are *different SQL*. PG and
//! SQLite share `ON CONFLICT … DO UPDATE … WHERE`; MySQL has no WHERE
//! on `ON DUPLICATE KEY UPDATE`, so it runs `INSERT IGNORE` followed by
//! a conditional `UPDATE`. An untested dialect arm here means
//! `DistributedLock` silently loses mutual exclusion on that backend —
//! which is the bug this fixes.
//!
//! ```bash
//! DATABASE_URL=postgres://rustango:rustango@127.0.0.1:5433/rustango_test \
//! MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/rustango_test \
//!   cargo test -p rustango --all-features --test cache_db_backend_add_live
//! ```
//!
//! Each backend skips silently when its env var is unset.

use std::sync::Arc;
use std::time::Duration;

use rustango::cache::{Cache, DatabaseCache};
use rustango::sql::Pool;

/// Shared body: every assertion that must hold on every dialect.
async fn assert_add_semantics(pool: Pool, table: &str) {
    let cache = DatabaseCache::new(pool, table);
    // Start from a known-empty table — a previous run's rows would
    // otherwise make the first `add` look like a loser.
    let _ = cache.drop_table().await;
    cache.ensure_table().await.expect("ensure_table");

    // 1. set-if-absent
    assert!(
        cache.add("lock:job", "token-a", None).await.unwrap(),
        "{table}: first add must take the key"
    );
    assert!(
        !cache.add("lock:job", "token-b", None).await.unwrap(),
        "{table}: second add must be refused while live"
    );
    assert_eq!(
        cache.get("lock:job").await.unwrap().as_deref(),
        Some("token-a"),
        "{table}: the loser must not overwrite the winner"
    );

    // 2. an expired entry is reclaimable (else a dead holder wedges
    //    the lock forever)
    assert!(cache
        .add("lock:short", "a", Some(Duration::from_millis(150)))
        .await
        .unwrap());
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(
        cache
            .add("lock:short", "c", Some(Duration::from_secs(60)))
            .await
            .unwrap(),
        "{table}: expired entry must be reclaimable"
    );

    // 3. `expires = 0` means never — a persistent entry is never stolen
    assert!(cache.add("persist", "first", None).await.unwrap());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !cache.add("persist", "second", None).await.unwrap(),
        "{table}: a no-TTL entry must never look expired"
    );

    // 4. the real scenario: concurrent acquirers, exactly one winner.
    //    Real client-side concurrency here, unlike SQLite's serialised
    //    writer — this is the assertion that actually exercises the
    //    database's row locking.
    let cache = Arc::new(cache);
    let mut handles = Vec::new();
    for i in 0..16 {
        let c = Arc::clone(&cache);
        handles.push(tokio::spawn(async move {
            c.add("lock:contended", &format!("t{i}"), None)
                .await
                .unwrap_or(false)
        }));
    }
    let mut winners = 0;
    for h in handles {
        if h.await.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "{table}: exactly one concurrent acquirer may win; {winners} won"
    );

    let _ = cache.drop_table().await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn add_is_atomic_on_postgres() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = Pool::connect(&url).await.expect("connect DATABASE_URL");
    assert_add_semantics(pool, "rustango_cache_add_pg").await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn add_is_atomic_on_mysql() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        return;
    };
    let pool = Pool::connect(&url).await.expect("connect MYSQL_TEST_URL");
    assert_add_semantics(pool, "rustango_cache_add_my").await;
}
