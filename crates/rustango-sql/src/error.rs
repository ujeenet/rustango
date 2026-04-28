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

    /// `InsertQuery` had no columns — Postgres does not accept zero-column inserts.
    #[error("INSERT requires at least one column")]
    EmptyInsert,

    /// `InsertQuery.columns.len() != InsertQuery.values.len()`.
    #[error("INSERT columns ({columns}) and values ({values}) length mismatch")]
    InsertShapeMismatch { columns: usize, values: usize },

    /// `UpdateQuery` had no assignments — `UPDATE ... SET` requires at least one.
    #[error("UPDATE requires at least one assignment in `set`")]
    EmptyUpdateSet,
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

    /// `insert_returning` was called with an `InsertQuery` carrying no
    /// `RETURNING` columns. Use `insert` for those.
    #[error("`insert_returning` requires `query.returning` to be non-empty; use `insert` instead")]
    EmptyReturning,
}
