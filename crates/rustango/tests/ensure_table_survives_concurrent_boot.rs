//! `ensure_table_pool` must tolerate another process doing the same
//! thing at the same moment (#1458).
//!
//! `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` are
//! **not atomic** on PostgreSQL: two sessions can both pass the
//! existence check and then both try to insert the catalogue row. The
//! loser gets an error even though the object it asked for now exists.
//!
//! That is precisely what the documented web + worker topology does —
//! both processes call `DatabaseJobQueue::ensure_table_pool` at boot.
//! Before the fix the loser's error propagated and killed the process;
//! under a container restart policy the only evidence was a restart
//! count:
//!
//! ```text
//! web-single-pg      restarts=1
//! worker-single-pg   restarts=0
//! ```
//!
//! Found by the commerce soak fleet (`docker/soak/`) on its first cold
//! start, and again on an independent one.
//!
//! The test drives real concurrency rather than asserting on the
//! predicate: a predicate test would pass while the caller still
//! propagated the error.

#![cfg(feature = "jobs-postgres")]

use rustango::jobs::DatabaseJobQueue;
use rustango::sql::Pool;

/// Skip when unconfigured; **fail loudly** when configured but
/// unreachable (#1440).
async fn pool_or_skip() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    if !url.starts_with("postgres") {
        return None;
    }
    match Pool::connect(&url).await {
        Ok(p) => Some(p),
        Err(e) => panic!("DATABASE_URL is set but unreachable ({url}) — driver said: {e}"),
    }
}

/// Many tasks calling `ensure_table_pool` at once, against a database
/// where the table does not yet exist. Every one must succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_ensure_table_all_succeed() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL unset or not postgres");
        return;
    };

    // Start from nothing, so every task races to create rather than
    // finding the object already there — which is the only window the
    // bug lives in.
    rustango::sql::raw_execute_pool(&pool, "DROP TABLE IF EXISTS rustango_jobs", Vec::new())
        .await
        .expect("drop");

    let tasks: Vec<_> = (0..12)
        .map(|_| {
            let p = pool.clone();
            tokio::spawn(async move { DatabaseJobQueue::ensure_table_pool(&p).await })
        })
        .collect();

    let mut failures = Vec::new();
    for (i, t) in tasks.into_iter().enumerate() {
        match t.await.expect("task panicked") {
            Ok(()) => {}
            Err(e) => failures.push(format!("task {i}: {e}")),
        }
    }

    assert!(
        failures.is_empty(),
        "ensure_table_pool must be safe to call from several processes at once — \
         `CREATE INDEX IF NOT EXISTS` is not atomic on Postgres, so the loser of \
         the race errors even though the index now exists (#1458). Failures:\n  {}",
        failures.join("\n  ")
    );

    // And it really did build the thing, rather than swallowing an
    // error that happened to mean something else. Without this the test
    // would pass if `ensure_table_pool` silently did nothing.
    let rows: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT COUNT(*)::bigint FROM pg_class WHERE relname = 'rustango_jobs_pickup_idx'",
        Vec::new(),
        &pool,
    )
    .await
    .expect("count the index");
    assert_eq!(
        rows.first().map(|r| r.0),
        Some(1),
        "the pickup index must exist exactly once after the race resolves"
    );
}

/// The predicate must not swallow an ordinary unique violation.
///
/// `23505` is `unique_violation` generally; only the one raised against
/// `pg_class_relname_nsp_index` is the catalogue race. Swallowing the
/// rest would hide real data errors — which would be a worse bug than
/// the one being fixed.
#[tokio::test]
async fn an_ordinary_unique_violation_is_still_an_error() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL unset or not postgres");
        return;
    };

    rustango::sql::raw_execute_pool(&pool, "DROP TABLE IF EXISTS dup_probe_1458", Vec::new())
        .await
        .expect("drop");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE dup_probe_1458 (k TEXT PRIMARY KEY)",
        Vec::new(),
    )
    .await
    .expect("create");

    let insert = "INSERT INTO dup_probe_1458 (k) VALUES ('same')";
    rustango::sql::raw_execute_pool(&pool, insert, Vec::new())
        .await
        .expect("first insert");
    let second = rustango::sql::raw_execute_pool(&pool, insert, Vec::new()).await;

    assert!(
        second.is_err(),
        "a duplicate primary key must still be an error — narrowing the swallowed \
         23505 to `pg_class_relname_nsp_index` is what keeps that true"
    );

    let _ = rustango::sql::raw_execute_pool(&pool, "DROP TABLE dup_probe_1458", Vec::new()).await;
}
