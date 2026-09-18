//! Migration errors.

use crate::sql::sqlx;

/// Raised while building or applying DDL, or while reading/writing
/// migration files on disk.
///
/// `#[non_exhaustive]`: end a match on this with `_ =>`. It gained
/// [`Self::PartiallyApplied`] in 0.58, and the marker is here so the
/// next variant is not another breaking change (#1513).
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
    /// Bubbled up from the executor — e.g. when the bi-dialect
    /// `_pool` runner functions ([`apply_all_pool`],
    /// [`drop_all_pool`]) dispatch through `raw_execute_pool`.
    /// Carries the underlying SQL writer / driver error.
    ///
    /// [`apply_all_pool`]: super::runner::apply_all_pool
    /// [`drop_all_pool`]: super::runner::drop_all_pool
    #[error(transparent)]
    Exec(#[from] crate::sql::ExecError),

    /// A migration failed **after** committing DDL that cannot be
    /// rolled back, so the database moved and the ledger did not.
    ///
    /// MySQL auto-commits on every DDL statement, so the transaction
    /// wrapping an `atomic: true` migration protects only the
    /// `RunSQL` / `RunPython` operations between them. When operation
    /// *k* of *n* fails and some earlier operation was DDL, the
    /// earlier DDL stays applied while the ledger row is never
    /// written.
    ///
    /// That state does not resolve by re-running: the migration
    /// replays from the top and fails differently, because the work it
    /// already did is still there. Reported from a live tenant as
    /// `DropTable` succeeding, the next operation failing, and the
    /// re-run then reporting `1051 Unknown table` (#1588).
    ///
    /// The `Display` names the counts and the recovery path, because
    /// the recovery — inspect the schema, then `migrate --fake <name>`
    /// once it matches — is otherwise folklore.
    ///
    /// Raised **only** when DDL actually committed before the failure.
    /// A migration whose failing operation is preceded solely by
    /// `RunSQL` rolls back cleanly and surfaces its driver error
    /// unchanged.
    #[error(
        "migration `{migration}` failed after completing {applied} of {total} operations, \
         having already committed {ddl_applied} DDL statement(s).\n\
         MySQL commits DDL immediately, so those {ddl_applied} are still applied and the \
         transaction could not undo them. The ledger row was not written, so re-running \
         replays from the top and will fail differently.\n\
         Recovery: inspect the schema against this migration, then \
         `manage migrate --fake {migration}` once it matches (add `--all-tenants` under \
         tenancy).\n\
         Cause: {source}"
    )]
    PartiallyApplied {
        /// The migration that moved the schema without recording it.
        migration: String,
        /// Operations that ran to completion before the failure.
        applied: usize,
        /// Operations in the migration.
        total: usize,
        /// DDL **statements** committed before the failure — counted
        /// per statement rather than per operation, because one
        /// operation can render several and each commits on its own,
        /// so an operation that fails halfway still leaves the earlier
        /// statements applied. This is the number that makes the state
        /// stuck, and it is why the variant is raised at all: at zero,
        /// the transaction rolled back cleanly.
        ddl_applied: usize,
        /// The driver error that stopped it.
        source: Box<sqlx::Error>,
    },
}
