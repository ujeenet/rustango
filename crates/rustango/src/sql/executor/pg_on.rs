//! PG-typed `_on` CRUD family — `insert_on` / `update_on` / `delete_on`
//! / `select_rows_on` / `insert_returning_on` / `bulk_insert_on`.
//!
//! These accept any sqlx executor (`&PgPool`, `&mut PgConnection`, or
//! a `Transaction`). Tenant-scoped writes need this: schema-mode
//! tenants share the registry pool and rely on the per-checkout
//! `SET search_path`, so passing a fresh `&PgPool` would silently
//! hit the wrong schema.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 9. PG-only
//! (parent `mod pg_on;` is gated `#[cfg(feature = "postgres")]`),
//! so the per-function cfg gates in the original block are dropped.

use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;

use super::bind_query;
use super::ExecError;
use super::Postgres;
use crate::core::{BulkInsertQuery, DeleteQuery, InsertQuery, SelectQuery, UpdateQuery};
use crate::sql::Dialect as _;

/// Like [`insert_pool`](crate::sql::insert_pool) but accepts any sqlx
/// executor — `&PgPool`, `&mut PgConnection`, or a transaction.
/// Tenant-scoped writes need this: schema-mode tenants share the
/// registry pool and rely on the per-checkout `SET search_path`, so
/// passing `&PgPool` would silently hit the wrong schema.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn insert_on<'c, E>(executor: E, query: &InsertQuery) -> Result<(), ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    query.validate()?;
    let stmt = Postgres.compile_insert(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    q.execute(executor).await?;
    Ok(())
}

/// Like [`insert_returning_pool`](crate::sql::insert_returning_pool)
/// but accepts any sqlx executor.
///
/// # Errors
/// [`ExecError`] if `returning` is empty, the query is invalid, or the
/// driver rejects it.
pub async fn insert_returning_on<'c, E>(
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

/// Like [`bulk_insert_pool`](crate::sql::bulk_insert_pool) on a pool,
/// `&mut PgConnection` or transaction. Past the bind cap it writes one
/// statement per chunk inside a transaction (a savepoint when already in
/// one), so the insert stays all-or-nothing. Returned rows keep row order.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn bulk_insert_on<'c, A>(
    conn: A,
    query: &BulkInsertQuery,
) -> Result<Vec<PgRow>, ExecError>
where
    A: sqlx::Acquire<'c, Database = sqlx::Postgres>,
{
    let mut stmts = super::compile_bulk_insert_batches(&Postgres, query)?;
    let returning = !query.returning.is_empty();
    let mut conn = conn.acquire().await?;
    match stmts.len() {
        0 => Ok(Vec::new()),
        1 => run_bulk_insert(&mut *conn, stmts.remove(0), returning).await,
        _ => {
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            let mut rows = Vec::new();
            for stmt in stmts {
                rows.extend(run_bulk_insert(&mut *tx, stmt, returning).await?);
            }
            tx.commit().await?;
            Ok(rows)
        }
    }
}

async fn run_bulk_insert(
    conn: &mut sqlx::PgConnection,
    stmt: crate::sql::CompiledStatement,
    returning: bool,
) -> Result<Vec<PgRow>, ExecError> {
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    if returning {
        Ok(q.fetch_all(conn).await?)
    } else {
        q.execute(conn).await?;
        Ok(Vec::new())
    }
}

/// Like [`update_pool`](crate::sql::update_pool) but accepts any sqlx
/// executor.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn update_on<'c, E>(executor: E, query: &UpdateQuery) -> Result<u64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    query.validate()?;
    let stmt = Postgres.compile_update(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let result = q.execute(executor).await?;
    Ok(result.rows_affected())
}

/// Like [`delete_pool`](crate::sql::delete_pool) but accepts any sqlx
/// executor.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn delete_on<'c, E>(executor: E, query: &DeleteQuery) -> Result<u64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    // Postgres folds the junction cleanup into this one statement.
    let plan = crate::sql::compile_delete_plan(&Postgres, query)?;
    // One statement here: delete as before; the cascade goes unaudited.
    if plan.cascade.is_some() {
        crate::audit::cascade::warn_unaudited(query.model.table);
    }
    let stmt = plan.owner;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let result = q.execute(executor).await?;
    Ok(result.rows_affected())
}

/// Like [`select_rows_pool`](crate::sql::select_rows_pool) but accepts
/// any sqlx executor — `&PgPool`, `&mut PgConnection`, or a
/// `Transaction`. Required for tenancy projects whose per-request
/// connection comes from [`crate::extractors::Tenant`] rather than a
/// single global pool.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn select_rows_on<'c, E>(
    executor: E,
    query: &SelectQuery,
) -> Result<Vec<PgRow>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_select(query)?;
    Ok(super::pg_on_query(&stmt.sql, stmt.params)
        .fetch_all(executor)
        .await?)
}
