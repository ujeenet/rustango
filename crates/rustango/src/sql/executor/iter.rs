//! `ChunkedIter<T>` — async iterator that streams a `QuerySet`'s
//! result set in fixed-size chunks (issue #23).
//!
//! Extracted from `executor/mod.rs` as part of #116 step 2.

use std::collections::VecDeque;
use std::marker::PhantomData;

use super::{select_rows_keyed_on, select_rows_pool_with_related, DecodeModel, ExecError};
use crate::core::{Filter, Model, NullsOrder, Op, OrderItem, SelectQuery, SqlValue};
use crate::sql::Pool;

/// Chunked async iterator over a compiled query. Issue #23.
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
    /// The queryset's own `limit` as an end offset: no chunk reads past it.
    pub(super) end: Option<i64>,
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
    /// Primary key of the last row read by keyset, inside an `atomic` block.
    pub(super) last_key: Option<SqlValue>,
    pub(super) _model: PhantomData<fn() -> T>,
}

impl<T> ChunkedIter<T>
where
    T: Model + DecodeModel,
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
        let size = self.end.map_or(self.chunk_size, |end| {
            self.chunk_size.min(end - self.offset)
        });
        if size <= 0 {
            self.exhausted = true;
            return Ok(None);
        }
        let rows = if super::in_block(pool) {
            self.keyset_chunk(pool, size).await?
        } else {
            let mut q = self.query.clone();
            // A set operation's window is on the merged result, not its head.
            let (limit, offset) = if q.compound.is_empty() {
                (&mut q.limit, &mut q.offset)
            } else {
                (&mut q.compound_limit, &mut q.compound_offset)
            };
            *limit = Some(size);
            *offset = Some(self.offset);
            select_rows_pool_with_related::<T>(pool, &q).await?
        };
        let n = rows.len() as i64;
        if rows.is_empty() {
            self.exhausted = true;
            return Ok(None);
        }
        if n < size {
            self.exhausted = true;
        }
        self.offset += n;
        Ok(Some(rows))
    }

    /// The next `size` rows after the last key read, by primary key. Inside
    /// a block the loop's own writes are visible, so an OFFSET would skip
    /// rows it moved out of the filter.
    async fn keyset_chunk(&mut self, pool: &Pool, size: i64) -> Result<Vec<T>, ExecError> {
        let refused = ExecError::InAtomic {
            op: "`iterator()` not ordered by the primary key",
            reason: "rows the loop changes could be skipped; order by the primary key only \
                     and keep it in the projection",
        };
        let mut q: SelectQuery = self.query.clone();
        let Some(pk) = q.model.primary_key().filter(|pk| {
            q.compound.is_empty() && q.projection.as_ref().is_none_or(|p| p.contains(&pk.column))
        }) else {
            return Err(refused);
        };
        let desc = match q.order_by.as_slice() {
            [] => false,
            [OrderItem::Column {
                column,
                desc,
                nulls: NullsOrder::Default,
            }] if *column == pk.column => *desc,
            _ => return Err(refused),
        };
        q.order_by = vec![OrderItem::Column {
            column: pk.column,
            desc,
            nulls: NullsOrder::Default,
        }];
        q.limit = Some(size);
        match self.last_key.clone() {
            // The queryset's own offset applies before the first key.
            None => q.offset = Some(self.offset),
            Some(key) => {
                q.offset = None;
                let op = if desc { Op::Lt } else { Op::Gt };
                q.where_clause
                    .push_and(Filter::new(pk.column, op, key).into());
            }
        }
        let rows = select_rows_keyed_on::<T, _>(pool, &q, (pk.column, Some(pk.ty))).await?;
        if let Some((key, _)) = rows.last() {
            self.last_key = Some(key.clone());
        }
        Ok(rows.into_iter().map(|(_, row)| row).collect())
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
