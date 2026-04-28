//! Migration errors.

use rustango_sql::sqlx;

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
}
