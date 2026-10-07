//! Migration errors.

use crate::sql::sqlx;

/// Raised while building or applying DDL, or while reading/writing
/// migration files on disk.
///
/// `#[non_exhaustive]`, so end a match on this with `_ =>`. New
/// variants such as [`Self::PartiallyApplied`] can then be added
/// without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrateError {
    #[error(transparent)]
    Driver(#[from] sqlx::Error),
    #[error("migration file I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("migration file JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Internal-consistency failure on a migration file (e.g. a
    /// `DataOp` flagged `reversible: true` with no `reverse_sql`).
    #[error("invalid migration: {0}")]
    Validation(String),
    /// From the executor, for example when [`apply_all_pool`] or
    /// [`drop_all_pool`] run SQL through `raw_execute_pool`.
    ///
    /// [`apply_all_pool`]: super::runner::apply_all_pool
    /// [`drop_all_pool`]: super::runner::drop_all_pool
    #[error(transparent)]
    Exec(#[from] crate::sql::ExecError),
    /// Another run held the migrate lock longer than
    /// [`with_lock_timeout`](super::with_lock_timeout) allowed.
    #[error("another run held the migrate lock for over {0:?}; nothing was applied")]
    LockTimeout(std::time::Duration),

    /// A migration failed **after** MySQL committed part of it. The
    /// database moved and the ledger did not.
    ///
    /// MySQL commits the open transaction at the first DDL statement,
    /// even one that fails, and runs later operations in autocommit. So
    /// the transaction around an `atomic: true` migration only protects
    /// the data operations before the first DDL.
    ///
    /// **Re-running does not fix this.** The migration replays from
    /// the top and fails in a new way, because its earlier work is
    /// still there. Recovery: compare the schema against the
    /// migration, then run `manage migrate --fake <name>` once they
    /// match.
    ///
    /// Raised **only** when something really committed. A failure
    /// before the first DDL statement rolls back and returns the
    /// driver error unchanged.
    #[error(
        "migration `{migration}` failed after committing {applied} of {total} operations \
         and {ddl_applied} DDL statement(s).\n\
         MySQL commits the open transaction at the first DDL statement, so that work is \
         still applied and could not be undone. The ledger row was not written, so \
         re-running replays from the top and will fail differently.\n\
         Recovery: inspect the database against this migration, then \
         `manage migrate --fake {migration}` once it matches (add `--all-tenants` under \
         tenancy).\n\
         Cause: {source}"
    )]
    PartiallyApplied {
        /// The migration that moved the schema without recording it.
        migration: String,
        /// Operations that ran to completion, and so committed, before
        /// the failure.
        applied: usize,
        /// Operations in the migration.
        total: usize,
        /// DDL **statements** committed before the failure. Counted
        /// per statement, not per operation: one operation can render
        /// several, and each commits on its own. Can be zero when only
        /// data operations committed (#2151).
        ddl_applied: usize,
        /// The driver error that stopped it.
        source: Box<sqlx::Error>,
    },
}
