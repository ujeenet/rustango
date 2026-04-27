//! SQL writer & executor errors.

use rustango_core::QueryError;

/// Raised while lowering a `SelectQuery` to a parameterized statement.
#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    /// `Op::In` was used with something other than `SqlValue::List`.
    #[error("`Op::In` requires `SqlValue::List`")]
    InRequiresList,

    /// `Op::IsNull` was used with something other than `SqlValue::Bool`.
    #[error("`Op::IsNull` requires `SqlValue::Bool` (true = IS NULL, false = IS NOT NULL)")]
    IsNullRequiresBool,

    /// `Op::In` with an empty list — Postgres does not accept `IN ()`.
    #[error("empty `IN` list is not supported")]
    EmptyInList,
}

/// Raised while compiling, writing, or executing a query end-to-end.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Query(#[from] QueryError),

    #[error(transparent)]
    Sql(#[from] SqlError),

    #[error(transparent)]
    Driver(#[from] sqlx::Error),
}
