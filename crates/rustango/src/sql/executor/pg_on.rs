//! PG-typed `_on` CRUD family — `insert_on` / `update_on` / `delete_on`
//! / `select_rows_on` / `select_one_row_on` / `insert_returning_on` /
//! `bulk_insert_on`.
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

/// Like [`insert`] but accepts any sqlx executor — `&PgPool`,
/// `&mut PgConnection`, or a transaction. Tenant-scoped writes need
/// this: schema-mode tenants share the registry pool and rely on the
/// per-checkout `SET search_path`, so passing `&PgPool` would silently
/// hit the wrong schema.
///
/// # Errors
/// As [`insert`].
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

/// Like [`insert_returning`] but accepts any sqlx executor.
///
/// # Errors
/// As [`insert_returning`].
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

/// Like [`bulk_insert`] but accepts any sqlx executor.
///
/// # Errors
/// As [`bulk_insert`].
pub async fn bulk_insert_on<'c, E>(
    executor: E,
    query: &BulkInsertQuery,
) -> Result<Vec<PgRow>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    query.validate()?;
    let stmt = Postgres.compile_bulk_insert(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    if query.returning.is_empty() {
        q.execute(executor).await?;
        Ok(Vec::new())
    } else {
        Ok(q.fetch_all(executor).await?)
    }
}

/// Like [`update`] but accepts any sqlx executor.
///
/// # Errors
/// As [`update`].
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

/// Like [`delete`] but accepts any sqlx executor.
///
/// # Errors
/// As [`delete`].
pub async fn delete_on<'c, E>(executor: E, query: &DeleteQuery) -> Result<u64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_delete(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let result = q.execute(executor).await?;
    Ok(result.rows_affected())
}

/// Like [`select_rows`] but accepts any sqlx executor — `&PgPool`,
/// `&mut PgConnection`, or a `Transaction`. Required for tenancy
/// projects whose per-request connection comes from
/// [`crate::extractors::Tenant`] rather than a single global pool.
///
/// # Errors
/// As [`select_rows`].
pub async fn select_rows_on<'c, E>(
    executor: E,
    query: &SelectQuery,
) -> Result<Vec<PgRow>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_select(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    Ok(q.fetch_all(executor).await?)
}

/// Like [`select_one_row`] but accepts any sqlx executor.
///
/// # Errors
/// As [`select_one_row`].
pub async fn select_one_row_on<'c, E>(
    executor: E,
    query: &SelectQuery,
) -> Result<Option<PgRow>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_select(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    Ok(q.fetch_optional(executor).await?)
}
