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

/// Expired rows go on the next write, so the table stays bounded.
async fn expired_ids_are_pruned(pool: &Pool) {
    let store = DbJtiStore::new(pool.clone());
    assert!(store.mark_used("old", now() - 10).await);
    assert!(store.mark_used("new", now() + 60).await);
    assert!(!store.is_used("old").await);
    assert!(store.is_used("new").await);
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
        expired_ids_are_pruned,
        an_overlong_id_is_refused,
        a_missing_table_refuses_tokens,
    ],
}
