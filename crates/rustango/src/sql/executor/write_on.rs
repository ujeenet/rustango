//! Writes on any [`SqlExecutor`]: the one statement-level path the
//! derive's `_on` and `_tx` methods share.

use super::{write_on, ExecError, InsertReturningPool, SqlExecutor};
use crate::core::{BulkInsertQuery, DeleteQuery, InsertQuery, UpdateQuery};

/// INSERT `query` on `executor`.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn insert_on<'c, E: SqlExecutor<'c>>(
    executor: E,
    query: &InsertQuery,
) -> Result<(), ExecError> {
    query.validate()?;
    let stmt = executor.dialect().compile_insert(query)?;
    write_on(executor, &stmt.sql, stmt.params).await.map(drop)
}

/// INSERT `query` on `executor` and read back its `RETURNING` row (MySQL:
/// the new auto id).
///
/// # Errors
/// [`ExecError::EmptyReturning`] without a `RETURNING` list, else as
/// [`insert_on`].
pub async fn insert_returning_on<'c, E: SqlExecutor<'c>>(
    executor: E,
    query: &InsertQuery,
) -> Result<InsertReturningPool, ExecError> {
    if query.returning.is_empty() {
        return Err(ExecError::EmptyReturning);
    }
    query.validate()?;
    executor.insert_returning(query.clone()).await
}

/// UPDATE `query` on `executor`; returns the rows it changed.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn update_on<'c, E: SqlExecutor<'c>>(
    executor: E,
    query: &UpdateQuery,
) -> Result<u64, ExecError> {
    query.validate()?;
    let stmt = executor.dialect().compile_update(query)?;
    write_on(executor, &stmt.sql, stmt.params).await
}

/// DELETE `query` on `executor`, with its junction and cascade statements.
/// A raw sqlx executor runs only the one DELETE, so a cascade into an
/// audited model gets no audit rows there.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn delete_on<'c, E: SqlExecutor<'c>>(
    executor: E,
    query: &DeleteQuery,
) -> Result<u64, ExecError> {
    let plan = crate::sql::compile_delete_plan(executor.dialect(), query)?;
    let single = plan.links.is_empty() && !executor.runs_plans();
    if plan.is_one_statement() || single {
        if plan.cascade.is_some() {
            crate::audit::cascade::warn_unaudited(query.model.table);
        }
        return write_on(executor, &plan.owner.sql, plan.owner.params).await;
    }
    executor.run_delete_plan(plan).await
}

/// Bulk INSERT `query` on `executor`, one statement per bind-sized chunk
/// in one transaction; returns its `RETURNING` rows in row order.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it; rows
/// asked of MySQL, or several chunks on a raw sqlx executor, are refused.
pub async fn bulk_insert_on<'c, E: SqlExecutor<'c>>(
    executor: E,
    query: &BulkInsertQuery,
) -> Result<Vec<InsertReturningPool>, ExecError> {
    if query.rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = query.clone();
    // MySQL has no RETURNING: write the rows, read nothing back.
    if !executor.dialect().supports_returning() {
        query.returning.clear();
    }
    executor.bulk_insert(query).await
}
