//! Count the SQL queries executed inside a scoped async block, then
//! assert the count matches an expectation. Use it to pin down N+1
//! regressions in a test.
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
//! Every `_pool` and `_tx` entry point in [`crate::sql`] that hits a
//! real query:
//!
//! - `raw_execute_pool` / `raw_query_pool` — fall-through raw SQL
//! - `select_rows_as_json` / `select_one_row_as_json` — JSON-bridge reads
//! - `select_rows_pool` / `select_rows_pool_with_related` /
//!   `select_one_row_pool` — typed reads
//! - `count_rows_pool`, `fetch_aggregate_pool`, `fetch_paginated_pool`
//! - `insert_pool` / `update_pool` / `delete_pool` — single-row writes
//! - the `_tx` counterparts of all of the above
//!
//! Each call increments by 1 regardless of how many rows the query
//! returns: one SQL statement is one count, even when it returns
//! thousands of rows.
//!
//! ## What does **not** get counted
//!
//! The PostgreSQL-only `_on` family — `annotate_count_children_on`,
//! `fetch_aggregate_on`, `fetch_with_prefetch`, `QuerySet::fetch_on`
//! — takes a bare sqlx executor rather than a [`crate::sql::Pool`] and
//! runs its query without passing through any instrumented entry
//! point. A block that only uses those counts zero.
//!
//! Stated here because the failure mode is a **pass**: `assert_num_queries`
//! sees no query and agrees with any expectation of 0. Until #1561, read
//! a 0 from a block that touched `_on` code as "not measured", not as
//! "no queries".

use std::cell::Cell;
use std::future::Future;

/// One scope's counts: `current` resets on [`QueryCounter::take`],
/// `total` does not, so an enclosing scope still sees every query.
#[derive(Default)]
struct Counts {
    current: Cell<usize>,
    total: Cell<usize>,
}

impl Counts {
    fn add(&self, n: usize) {
        self.current.set(self.current.get() + n);
        self.total.set(self.total.get() + n);
    }
}

tokio::task_local! {
    /// Per-task SQL query counter. `None`-equivalent (the `try_with`
    /// returns `Err`) outside an active scope, which is the production
    /// path — every `_pool` call's `bump()` is a no-op.
    static COUNTER: Counts;
}

/// Bump the per-task query counter by 1. No-op when called outside an
/// active [`assert_num_queries`] or [`QueryCounter::scope`] block, so
/// production code paths pay zero runtime cost.
///
/// Called by every `_pool` entry point in [`crate::sql::executor`].
pub(crate) fn bump() {
    let _ = COUNTER.try_with(|c| c.add(1));
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
        let (out, total) = COUNTER
            .scope(Counts::default(), async {
                let out = fut.await;
                (out, COUNTER.with(|c| c.total.get()))
            })
            .await;
        // A nested scope's queries also count for the one around it (#1960).
        let _ = COUNTER.try_with(|c| c.add(total));
        out
    }

    /// Read the current count inside an active scope. Panics if
    /// called outside [`Self::scope`] / [`assert_num_queries`] — the
    /// caller should only invoke it inside a guarded block.
    #[must_use]
    pub fn current() -> usize {
        COUNTER
            .try_with(|c| c.current.get())
            .expect("QueryCounter::current() called outside an active scope")
    }

    /// Read the count and reset to 0 atomically. Useful when one test
    /// runs multiple unrelated operations and wants to count each
    /// segment independently.
    pub fn take() -> usize {
        COUNTER
            .try_with(|c| c.current.replace(0))
            .expect("QueryCounter::take() called outside an active scope")
    }
}

/// Run `fut` and assert that exactly `expected` SQL queries executed
/// during it. Panics on mismatch, printing both counts.
///
/// Returns the future's output, so the caller can go on to assert on
/// the produced value.
///
/// # Panics
/// When the observed count differs from `expected`.
pub async fn assert_num_queries<F: Future>(expected: usize, fut: F) -> F::Output {
    QueryCounter::scope(async move {
        let result = fut.await;
        let actual = QueryCounter::current();
        assert_eq!(
            actual, expected,
            "assert_num_queries failed: expected {expected} queries, observed {actual}"
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
    #[should_panic(expected = "assert_num_queries failed: expected 2 queries, observed 3")]
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

    /// The outer scope counts the inner one's queries, even ones it took (#1960).
    #[tokio::test]
    async fn nested_scope_counts_toward_the_outer_one() {
        assert_num_queries(3, async {
            bump();
            assert_num_queries(2, async {
                bump();
                bump();
            })
            .await;
        })
        .await;
        QueryCounter::scope(async {
            QueryCounter::scope(async {
                bump();
                assert_eq!(QueryCounter::take(), 1);
            })
            .await;
            assert_eq!(QueryCounter::current(), 1);
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
