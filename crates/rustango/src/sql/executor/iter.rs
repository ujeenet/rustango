//! `ChunkedIter<T>` — async iterator that streams a `QuerySet`'s
//! result set in fixed-size chunks (Django's `QuerySet.iterator()`,
//! issue #23).
//!
//! Extracted from `executor/mod.rs` as part of #116 step 2.

use std::collections::VecDeque;
use std::marker::PhantomData;

use super::{
    select_rows_pool_with_related, ExecError, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated,
    MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
};
use crate::core::Model;
use crate::sql::Pool;

/// Chunked async iterator over a compiled query — Django's
/// `QuerySet.iterator(chunk_size=...)`. Issue #23.
///
/// Constructed via [`crate::query::QuerySet::iterator`]. Internally
/// re-runs the underlying `SELECT` with rotating `OFFSET` per chunk
/// so the full result set never lives in memory at once. Two
/// consumption styles:
///
/// - [`Self::next_chunk`] yields `Option<Vec<T>>` — a whole batch at
///   a time. `None` means iteration is done.
/// - [`Self::next_row`] yields `Option<T>` one at a time, buffering
///   the current chunk internally between calls.
///
/// Mix freely on the same iterator — both methods share the same
/// buffer + offset state.
///
/// Iteration is **exhausted-aware**: once the underlying query
/// returns a chunk smaller than `chunk_size` (or no rows), the
/// iterator marks itself exhausted and every subsequent call to
/// `next_chunk` / `next_row` returns `Ok(None)` without re-querying.
pub struct ChunkedIter<T> {
    /// Compiled `SelectQuery` re-issued per chunk with rotating
    /// `OFFSET`. Cloned every chunk so the per-chunk `limit` /
    /// `offset` mutation doesn't mutate the original.
    pub(super) query: crate::core::SelectQuery,
    /// Per-chunk row cap. Picked at construction; doesn't change.
    pub(super) chunk_size: i64,
    /// Next `OFFSET` to issue on the next chunk fetch. Bumped by the
    /// number of rows actually returned (not by `chunk_size`) so a
    /// short final chunk doesn't skip ahead.
    pub(super) offset: i64,
    /// `true` once a fetched chunk came back smaller than
    /// `chunk_size`. Further `next_chunk` / `next_row` calls skip the
    /// DB and return `Ok(None)`.
    pub(super) exhausted: bool,
    /// One-chunk buffer for the row-by-row [`Self::next_row`] path.
    /// `VecDeque` so `pop_front` is O(1); a `Vec` would be O(n) per
    /// row. Empty when no rows are buffered or the iterator has been
    /// drained chunk-by-chunk via [`Self::next_chunk`].
    pub(super) buffer: VecDeque<T>,
    /// Cumulative row count yielded so far (across both `next_chunk`
    /// and `next_row` paths). Used for [`Self::rows_seen`].
    pub(super) seen: i64,
    pub(super) _model: PhantomData<fn() -> T>,
}

impl<T> ChunkedIter<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Fetch the next chunk. Returns `Ok(None)` when iteration is done.
    ///
    /// If `next_row` was previously called and left rows in the
    /// internal buffer, those rows are drained as the head of the
    /// returned chunk before any new DB query — mixing `next_row` and
    /// `next_chunk` on the same iterator preserves row order.
    ///
    /// # Errors
    /// As [`select_rows_pool_with_related`].
    pub async fn next_chunk(&mut self, pool: &Pool) -> Result<Option<Vec<T>>, ExecError> {
        // Drain any rows left in the per-row buffer first — they were
        // pre-fetched by an earlier `next_row` call and need to come
        // out before any new DB fetch.
        if !self.buffer.is_empty() {
            let buffered: Vec<T> = self.buffer.drain(..).collect();
            self.seen += buffered.len() as i64;
            return Ok(Some(buffered));
        }
        match self.fetch_next_chunk(pool).await? {
            Some(rows) => {
                self.seen += rows.len() as i64;
                Ok(Some(rows))
            }
            None => Ok(None),
        }
    }

    /// Yield one row at a time, buffering an internal chunk between
    /// calls. Returns `Ok(None)` when iteration is done.
    ///
    /// O(1) per row — uses `VecDeque::pop_front` against the internal
    /// buffer, refilling from the next chunk only when empty.
    ///
    /// # Errors
    /// As [`Self::next_chunk`].
    pub async fn next_row(&mut self, pool: &Pool) -> Result<Option<T>, ExecError> {
        if let Some(row) = self.buffer.pop_front() {
            self.seen += 1;
            return Ok(Some(row));
        }
        match self.fetch_next_chunk(pool).await? {
            Some(rows) => {
                self.buffer.extend(rows);
                let row = self.buffer.pop_front();
                if row.is_some() {
                    self.seen += 1;
                }
                Ok(row)
            }
            None => Ok(None),
        }
    }

    /// Shared fetch path for [`Self::next_chunk`] / [`Self::next_row`].
    /// Clones the compiled query, rotates `LIMIT`/`OFFSET` for this
    /// chunk, runs the fetch, advances `offset`, and flips the
    /// `exhausted` flag when the chunk comes back short or empty.
    /// Does NOT touch `self.seen` — that's the caller's
    /// responsibility because the row-by-row path counts as rows pop
    /// out of the buffer, not as the chunk arrives.
    async fn fetch_next_chunk(&mut self, pool: &Pool) -> Result<Option<Vec<T>>, ExecError> {
        if self.exhausted {
            return Ok(None);
        }
        // Clone so the per-chunk LIMIT/OFFSET tweaks don't mutate the
        // original (the iterator may be re-used across many chunks).
        let mut q = self.query.clone();
        q.limit = Some(self.chunk_size);
        q.offset = Some(self.offset);
        let rows = select_rows_pool_with_related::<T>(pool, &q).await?;
        let n = rows.len() as i64;
        if rows.is_empty() {
            self.exhausted = true;
            return Ok(None);
        }
        if n < self.chunk_size {
            self.exhausted = true;
        }
        self.offset += n;
        Ok(Some(rows))
    }

    /// Cumulative count of rows yielded so far across both
    /// `next_chunk` and `next_row` calls. Useful for progress
    /// reporting on long drains.
    #[must_use]
    pub fn rows_seen(&self) -> i64 {
        self.seen
    }

    /// `true` once iteration has been exhausted — every subsequent
    /// `next_chunk` / `next_row` call will return `Ok(None)` without
    /// hitting the database.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.exhausted && self.buffer.is_empty()
    }
}
