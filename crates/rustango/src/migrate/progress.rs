//! Watching a migration run while it happens.
//!
//! The runner used to be silent: it took the migrate lock, applied every
//! pending file, and returned a `Vec<Migration>` when it was done. On a
//! fresh tenant that is twenty-plus migrations of nothing, and a failure
//! reported only which *tenant* died, never which migration or how many
//! had already landed.
//!
//! An observer fixes that without changing what the runner does. Pass one
//! to [`migrate_pool_with_progress`](super::migrate_pool_with_progress)
//! and it is called as each migration starts and finishes; pass nothing
//! and the behaviour is exactly what it was.
//!
//! ## Observers must not block
//!
//! Events are emitted from **inside the migrate lock** — a PG advisory
//! lock, a MySQL `GET_LOCK`, held for the whole run. Every other process
//! trying to migrate is waiting behind it. An observer that blocks, does
//! synchronous I/O, or sends on a full unbuffered channel extends that
//! hold for everyone.
//!
//! Send on a bounded channel that drops when full, or push onto a queue a
//! separate task drains. Losing a progress event is a cosmetic problem;
//! stalling every migrating pod is not.
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
/// Owned rather than borrowed on purpose: an observer's whole job is
/// usually to hand the event to somewhere else — a channel, a broadcast
/// bus, a run log — and a borrow would tie it to the runner's stack. One
/// allocation per migration is not measurable against the DDL it
/// describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationEvent {
    /// The pending set has been computed. Emitted once, before any
    /// migration runs, so a watcher can size a progress bar — including
    /// when `total` is 0 and nothing else will follow.
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
        /// What the runner actually did — see [`Outcome`]. Not every
        /// "finished" migration ran any SQL.
        outcome: Outcome,
        /// Wall time for this migration alone. The reason to emit
        /// per-migration timings rather than a total: a forty-migration
        /// chain where one takes ninety seconds is exactly the thing
        /// this is meant to make visible.
        elapsed: Duration,
    },

    /// This migration failed. The runner stops here — a migration chain
    /// is ordered, so there is no useful sense in which the rest could
    /// be attempted — and no further events follow.
    Failed {
        name: String,
        index: usize,
        total: usize,
        /// Rendered rather than the live error: the error is returned to
        /// the caller by the runner, and an observer usually wants to
        /// display or store this rather than match on it.
        error: String,
    },
}

/// What the runner did with a migration it reports as finished.
///
/// Three genuinely different things wear the word "applied", and
/// collapsing them is how a watcher comes to believe DDL ran that never
/// did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Ran its `forward` operations, then recorded it.
    Ran,

    /// Ran, but skipped the operations for tables that already existed —
    /// the cross-ledger reconciliation path. The listed tables were left
    /// exactly as they were.
    RanPartial {
        /// Tables whose operations were skipped because they were
        /// already present.
        skipped: Vec<String>,
    },

    /// Recorded in the ledger **without running any DDL**: the end state
    /// was already present (a squash over history the database already
    /// has, or the guarded fake-initial path for a subsystem whose
    /// tables predate its migrations).
    ///
    /// Distinct from [`Ran`](Outcome::Ran) because an operator reading
    /// "applied 0003" and an operator reading "faked 0003" should reach
    /// different conclusions about what is in their database.
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
/// **Implementations must not block** — see the [module
/// docs](self#observers-must-not-block). Events arrive while the
/// migrate lock is held.
pub trait MigrationObserver: Send + Sync {
    /// Handle one event. Must return promptly and must not panic — a
    /// panic here unwinds through the runner with the migrate lock held.
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

/// Emit to an optional observer. `None` is the silent path every
/// pre-existing entry point takes, and compiles to nothing.
pub(super) fn emit(
    observer: Option<&dyn MigrationObserver>,
    event: impl FnOnce() -> MigrationEvent,
) {
    if let Some(observer) = observer {
        observer.on_event(event());
    }
}
