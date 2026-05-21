//! Django-shape `assertNumQueries` — count SQL queries executed inside
//! a scoped async block, then assert the count matches an expectation.
//! Django-parity #431.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::test_assertions::assert_num_queries;
//!
//! #[tokio::test]
//! async fn list_view_uses_exactly_two_queries() {
//!     let pool = make_pool().await;
//!     assert_num_queries(2, async {
//!         // 1: SELECT count(*) FROM posts (for pagination)
//!         // 2: SELECT * FROM posts LIMIT 20
//!         my_list_handler(&pool).await;
//!     })
//!     .await;
//! }
//! ```
//!
//! ## Scoped guard form
//!
//! For more control (multiple intermediate checks, custom messages),
//! use [`QueryCounter`] directly:
//!
//! ```ignore
//! use rustango::test_assertions::QueryCounter;
//!
//! QueryCounter::scope(async {
//!     read_some_data().await;
//!     assert_eq!(QueryCounter::current(), 1);
//!     write_some_data().await;
//!     assert_eq!(QueryCounter::current(), 2);
//! })
//! .await;
//! ```
//!
//! ## Semantics
//!
//! The counter is **per-task** via [`tokio::task_local!`] — calls from
//! tasks outside an active scope are no-ops, so production code paths
//! pay zero cost. Spawning a new task inside the scope does NOT
//! inherit the counter unless you propagate it manually (tokio
//! task-locals don't cross `spawn` boundaries).
//!
//! ## What gets counted
//!
//! Every `_pool` entry point in [`crate::sql`] that hits a real query:
//!
//! - `raw_execute_pool` — fall-through raw SQL
//! - `select_rows_as_json` / `select_one_row_as_json` — JSON-bridge reads
//! - `select_rows_pool_with_related` / `select_one_row_pool` — typed reads
//! - `insert_pool` / `update_pool` / `delete_pool` — single-row writes
//!
//! Each call increments by 1 regardless of how many rows the query
//! returns — mirroring Django's `assertNumQueries` (one SQL statement
//! = one count, even if it returns thousands of rows).

use std::cell::Cell;
use std::future::Future;

tokio::task_local! {
    /// Per-task SQL query counter. `None`-equivalent (the `try_with`
    /// returns `Err`) outside an active scope, which is the production
    /// path — every `_pool` call's `bump()` is a no-op.
    static COUNTER: Cell<usize>;
}

/// Bump the per-task query counter by 1. No-op when called outside an
/// active [`assert_num_queries`] or [`QueryCounter::scope`] block, so
/// production code paths pay zero runtime cost.
///
/// Called by every `_pool` entry point in [`crate::sql::executor`].
pub(crate) fn bump() {
    let _ = COUNTER.try_with(|c| c.set(c.get() + 1));
}

/// Scoped query counter. See module docs for the chained API
/// (`scope` / `current` / `take`).
pub struct QueryCounter;

impl QueryCounter {
    /// Run `fut` inside a fresh counter scope. Inside the scope, every
    /// `_pool` query increments the counter; [`Self::current`] reads
    /// it. The counter is dropped when the future returns.
    ///
    /// Use this when you want intermediate counts during the scope.
    /// For the simple "assert N total at the end" case use the
    /// top-level [`assert_num_queries`] helper.
    pub async fn scope<F: Future>(fut: F) -> F::Output {
        COUNTER.scope(Cell::new(0), fut).await
    }

    /// Read the current count inside an active scope. Panics if
    /// called outside [`Self::scope`] / [`assert_num_queries`] — the
    /// caller should only invoke it inside a guarded block.
    #[must_use]
    pub fn current() -> usize {
        COUNTER
            .try_with(Cell::get)
            .expect("QueryCounter::current() called outside an active scope")
    }

    /// Read the count and reset to 0 atomically. Useful when one test
    /// runs multiple unrelated operations and wants to count each
    /// segment independently.
    pub fn take() -> usize {
        COUNTER
            .try_with(|c| {
                let n = c.get();
                c.set(0);
                n
            })
            .expect("QueryCounter::take() called outside an active scope")
    }
}

/// Run `fut` and assert that exactly `expected` SQL queries executed
/// during it. Panics on mismatch with a Django-shape message.
///
/// Returns the future's output so callers can chain assertions on the
/// produced value the same way Django's `assertNumQueries` returns a
/// context manager that captures the wrapped code's return value.
///
/// # Panics
/// When the observed count differs from `expected`.
pub async fn assert_num_queries<F: Future>(expected: usize, fut: F) -> F::Output {
    QueryCounter::scope(async move {
        let result = fut.await;
        let actual = QueryCounter::current();
        assert_eq!(
            actual, expected,
            "assertNumQueries failed: expected {expected} queries, observed {actual}"
        );
        result
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bump_outside_scope_is_no_op() {
        // Calling bump() outside any scope must not panic.
        bump();
        bump();
        bump();
        // No way to observe — just confirming the no-op path.
    }

    #[tokio::test]
    async fn assert_num_queries_passes_on_exact_count() {
        assert_num_queries(3, async {
            bump();
            bump();
            bump();
        })
        .await;
    }

    #[tokio::test]
    async fn assert_num_queries_passes_on_zero_when_no_queries() {
        assert_num_queries(0, async {
            // No SQL — count stays 0.
            let _ = 1 + 1;
        })
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "assertNumQueries failed: expected 2 queries, observed 3")]
    async fn assert_num_queries_panics_with_count_in_message() {
        assert_num_queries(2, async {
            bump();
            bump();
            bump();
        })
        .await;
    }

    #[tokio::test]
    async fn returns_inner_future_output() {
        let value = assert_num_queries(1, async {
            bump();
            42
        })
        .await;
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn current_reads_running_count_mid_scope() {
        QueryCounter::scope(async {
            assert_eq!(QueryCounter::current(), 0);
            bump();
            assert_eq!(QueryCounter::current(), 1);
            bump();
            bump();
            assert_eq!(QueryCounter::current(), 3);
        })
        .await;
    }

    #[tokio::test]
    async fn take_resets_counter_atomically() {
        QueryCounter::scope(async {
            bump();
            bump();
            assert_eq!(QueryCounter::take(), 2);
            assert_eq!(QueryCounter::current(), 0);
            bump();
            assert_eq!(QueryCounter::take(), 1);
            assert_eq!(QueryCounter::current(), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn parallel_scopes_count_independently() {
        // Two simultaneous scopes do NOT see each other's bumps.
        let (a, b) = tokio::join!(
            assert_num_queries(2, async {
                bump();
                bump();
            }),
            assert_num_queries(5, async {
                for _ in 0..5 {
                    bump();
                }
            }),
        );
        assert_eq!(a, ());
        assert_eq!(b, ());
    }
}
