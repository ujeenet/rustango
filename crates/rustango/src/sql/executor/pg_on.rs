//! The Postgres `INSERT … RETURNING` on any sqlx executor, behind
//! [`super::SqlExecutor::insert_returning`].

use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;

use super::bind_query;
use super::ExecError;
use super::Postgres;
use crate::core::InsertQuery;
use crate::sql::Dialect as _;

/// Like [`insert_returning_pool`](crate::sql::insert_returning_pool)
/// but accepts any sqlx executor.
///
/// # Errors
/// [`ExecError`] if `returning` is empty, the query is invalid, or the
/// driver rejects it.
pub(crate) async fn pg_insert_returning<'c, E>(
    executor: E,
    query: &InsertQuery,
) -> Result<PgRow, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    if query.returning.is_empty() {
        return Err(ExecError::EmptyReturning);
    }
    query.validate()?;
    let stmt = Postgres.compile_insert(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let row = q.fetch_one(executor).await?;
    Ok(row)
}
