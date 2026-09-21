//! Watching a migration run while it happens.
//!
//! Without an observer the runner is silent until it finishes, so a
//! long run shows nothing and a failure does not say which migration
//! died. Pass an observer to
//! [`migrate_pool_with_progress`](crate::migrate::migrate_pool_with_progress)
//! and it is called as each migration starts and finishes.
//!
//! ## Observers must not block
//!
//! Events are emitted **while the migrate lock is held** (a PG advisory
//! lock, or `GET_LOCK` on MySQL), for the whole run. Every other
//! process that wants to migrate is waiting behind it. An observer that
//! blocks, does sync I/O, or sends on a full unbuffered channel makes
//! them all wait longer.
//!
//! Use a bounded channel that drops when full, or a queue another task
//! drains. Losing a progress event is cosmetic; stalling every
//! migrating process is not.
//!
//! ```ignore
//! use rustango::migrate::{migrate_pool_with_progress, MigrationEvent};
//!
//! migrate_pool_with_progress(&pool, dir, &|event: MigrationEvent| {
//!     if let MigrationEvent::Finished { name, index, total, .. } = &event {
//!         println!("[{index}/{total}] {name}");
//!     }
//! })
//! .await?;
//! ```

use std::time::Duration;

/// Something that happened during a migration run.
///
/// Fields are owned, not borrowed, so an observer can forward the event
/// to a channel or a log without tying it to the runner's stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationEvent {
    /// The pending set is known. Emitted once, before any migration
    /// runs, so a watcher can size a progress bar. Also emitted when
    /// `total` is 0 and nothing else will follow.
    Planned {
        /// How many migrations will be attempted.
        total: usize,
    },

    /// About to apply this migration.
    Started {
        /// File stem, e.g. `0002_add_bio_to_author`.
        name: String,
        /// 1-based position in the pending set.
        index: usize,
        total: usize,
    },

    /// This migration is done, and the run continues.
    Finished {
        name: String,
        index: usize,
        total: usize,
        /// What the runner did. See [`Outcome`]: not every finished
        /// migration ran SQL.
        outcome: Outcome,
        /// Wall time for this migration alone, so one slow migration
        /// in a long chain stands out.
        elapsed: Duration,
    },

    /// This migration failed. A chain is ordered, so the runner stops
    /// here and no further events follow.
    Failed {
        name: String,
        index: usize,
        total: usize,
        /// The error as text. The runner also returns the live error
        /// to its caller; an observer usually just displays this.
        error: String,
    },
}

/// What the runner did with a migration it reports as finished.
///
/// Three different things are all loosely called "applied". They are
/// kept apart so a watcher does not assume DDL ran when it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Ran its `forward` operations, then recorded it.
    Ran,

    /// Ran, but skipped the operations for tables that already
    /// existed. The listed tables were left exactly as they were.
    RanPartial {
        /// Tables whose operations were skipped because they were
        /// already present.
        skipped: Vec<String>,
    },

    /// Recorded in the ledger **without running any DDL**, because the
    /// end state was already there: a squash over history the database
    /// already has, or the fake-initial path for tables that predate
    /// their migrations.
    ///
    /// Kept apart from [`Ran`](Outcome::Ran) so "faked 0003" and
    /// "applied 0003" do not read the same in a log.
    Faked,
}

/// Receives [`MigrationEvent`]s as a run progresses.
///
/// Implemented for any `Fn(MigrationEvent)`, so the common case is a
/// closure:
///
/// ```ignore
/// migrate_pool_with_progress(&pool, dir, &|e| tx.try_send(e).ok()).await?;
/// ```
///
/// **Implementations must not block.** Events arrive while the migrate
/// lock is held. See the [module docs](self#observers-must-not-block).
pub trait MigrationObserver: Send + Sync {
    /// Handle one event. Must return quickly and must not panic: a
    /// panic here unwinds through the runner holding the migrate lock.
    fn on_event(&self, event: MigrationEvent);
}

impl<F> MigrationObserver for F
where
    F: Fn(MigrationEvent) + Send + Sync,
{
    fn on_event(&self, event: MigrationEvent) {
        self(event);
    }
}

/// Emit to an optional observer. `None` is the silent path and
/// compiles to nothing.
pub(super) fn emit(
    observer: Option<&dyn MigrationObserver>,
    event: impl FnOnce() -> MigrationEvent,
) {
    if let Some(observer) = observer {
        observer.on_event(event());
    }
}
