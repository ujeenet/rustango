//! Migration errors.

use rustango_sql::sqlx;

/// Raised while building or applying DDL.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Driver(#[from] sqlx::Error),
}
