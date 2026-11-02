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
use crate::core::{InsertQuery, SelectQuery};
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
