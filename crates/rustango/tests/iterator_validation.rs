//! Sync (no-DB) tests for `QuerySet::iterator` argument validation
//! (issue #23). Runtime semantics live in `iterator_live.rs`; this
//! file pins the construction-time guards.

use rustango::Model;

#[derive(Model)]
#[rustango(table = "iter_validation_row")]
#[allow(dead_code)]
pub struct Row {
    #[rustango(primary_key)]
    id: i64,
    value: i64,
}

/// `chunk_size <= 0` is almost always a programmer error (e.g.
/// `iterator(unchecked_user_input as i64)` where the input is 0 or
/// negative). Silently producing an immediately-exhausted iterator
/// would lose every row. Assert surfaces the misuse loudly.
#[test]
#[should_panic(expected = "chunk_size must be > 0")]
fn iterator_with_zero_chunk_size_panics() {
    let _ = Row::objects().iterator(0);
}

#[test]
#[should_panic(expected = "chunk_size must be > 0")]
fn iterator_with_negative_chunk_size_panics() {
    let _ = Row::objects().iterator(-100);
}

/// Positive chunk size goes through cleanly — no DB call yet, just
/// compiles the queryset.
#[test]
fn iterator_with_positive_chunk_size_compiles() {
    let r = Row::objects().iterator(2_000);
    assert!(r.is_ok(), "positive chunk_size compiles");
}
