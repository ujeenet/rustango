//! Migration errors.

use crate::sql::sqlx;

/// Raised while building or applying DDL, or while reading/writing
/// migration files on disk.
#[derive(Debug, thiserror::Error)]
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
}
