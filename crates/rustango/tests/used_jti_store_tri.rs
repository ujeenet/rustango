//! The registry-backed spent-token store: single use, pruned, and closed
//! when the table cannot be read, on every backend.

#![cfg(feature = "tenancy")]

use rustango::jti_store::JtiStore as _;
use rustango::sql::Pool;
use rustango::tenancy::{DbJtiStore, UsedJti};
use rustango::tri_dialect_test;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The first `mark_used` wins; every later one is a replay.
async fn a_token_id_is_spent_once(pool: &Pool) {
    let store = DbJtiStore::new(pool.clone());
    assert!(!store.is_used("a").await);
    assert!(store.mark_used("a", now() + 60).await);
    assert!(!store.mark_used("a", now() + 60).await, "a replay");
    assert!(store.is_used("a").await);
    // Another store on the same database sees it: a restart or a replica.
    assert!(DbJtiStore::new(pool.clone()).is_used("a").await);
    assert!(!store.is_used("b").await);
}

/// Rows expired past the margin go on a store's first write; a row just
/// expired stays, for a replica whose clock runs slower.
async fn expired_ids_are_pruned_past_a_margin(pool: &Pool) {
    let store = DbJtiStore::new(pool.clone());
    assert!(store.mark_used("old", now() - 400).await);
    assert!(store.mark_used("recent", now() - 10).await);
    // A fresh store (another process) prunes on its first write.
    assert!(
        DbJtiStore::new(pool.clone())
            .mark_used("new", now() + 60)
            .await
    );
    assert!(!store.is_used("old").await);
    assert!(store.is_used("recent").await);
    assert!(store.is_used("new").await);
    // `store` pruned on its first write; the next prune waits its interval.
    assert!(store.mark_used("old2", now() - 400).await);
    assert!(store.mark_used("later", now() + 60).await);
    assert!(store.is_used("old2").await, "one prune per interval");
}

/// An id longer than the column is refused, never truncated onto another.
async fn an_overlong_id_is_refused(pool: &Pool) {
    let store = DbJtiStore::new(pool.clone());
    let long = "x".repeat(129);
    assert!(!store.mark_used(&long, now() + 60).await);
}

/// A table that cannot be read counts every id as spent.
async fn a_missing_table_refuses_tokens(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, "rustango_used_jti").await;
    let store = DbJtiStore::new(pool.clone());
    assert!(store.is_used("a").await);
    assert!(!store.mark_used("a", now() + 60).await);
}

tri_dialect_test! {
    model: UsedJti,
    scenarios: [
        a_token_id_is_spent_once,
        expired_ids_are_pruned_past_a_margin,
        an_overlong_id_is_refused,
        a_missing_table_refuses_tokens,
    ],
}
