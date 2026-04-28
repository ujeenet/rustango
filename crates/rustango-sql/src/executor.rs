//! Async executor — binds a `CompiledStatement` to sqlx and runs it.

use rustango_core::{DeleteQuery, InsertQuery, Model, SelectQuery, SqlValue, UpdateQuery};
use rustango_query::{QuerySet, UpdateBuilder};
use sqlx::postgres::{PgArguments, PgPool, PgRow};
use sqlx::query::{Query, QueryAs};

use crate::{Dialect, ExecError, Postgres};

/// Extension trait that drives a `QuerySet` to completion against a Postgres pool.
///
/// Adds `.fetch(&pool)` to any `QuerySet<T>` whose `T` is `Model + FromRow`.
/// Pulled in via `use rustango::sql::Fetcher;`.
pub trait Fetcher<T>
where
    T: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    /// Compile the queryset, write Postgres SQL, and run `fetch_all`.
    ///
    /// # Errors
    /// Returns [`ExecError`] if any of the three stages fails: schema
    /// validation, SQL writing, or the underlying sqlx call.
    fn fetch(
        self,
        pool: &PgPool,
    ) -> impl std::future::Future<Output = Result<Vec<T>, ExecError>> + Send;
}

impl<T> Fetcher<T> for QuerySet<T>
where
    T: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    async fn fetch(self, pool: &PgPool) -> Result<Vec<T>, ExecError> {
        let select = self.compile()?;
        let stmt = Postgres.compile_select(&select)?;

        let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> = sqlx::query_as::<_, T>(&stmt.sql);
        for value in stmt.params {
            q = bind_query_as(q, value);
        }
        let rows = q.fetch_all(pool).await?;
        Ok(rows)
    }
}

/// Run an `InsertQuery` against a Postgres pool.
///
/// Validates each value against the declared field bounds (`max_length`,
/// `min`, `max`) before opening the connection.
///
/// # Errors
/// Returns [`ExecError`] for validation, SQL-writing, or driver failures.
pub async fn insert(pool: &PgPool, query: &InsertQuery) -> Result<(), ExecError> {
    query.validate()?;
    let stmt = Postgres.compile_insert(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    q.execute(pool).await?;
    Ok(())
}

/// Run an `UpdateQuery` against a Postgres pool. Returns rows affected.
///
/// Validates each `SET` value against the declared field bounds before
/// opening the connection.
///
/// # Errors
/// Returns [`ExecError`] for validation, SQL-writing, or driver failures.
pub async fn update(pool: &PgPool, query: &UpdateQuery) -> Result<u64, ExecError> {
    query.validate()?;
    let stmt = Postgres.compile_update(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let result = q.execute(pool).await?;
    Ok(result.rows_affected())
}

/// Run a `DeleteQuery` against a Postgres pool. Returns rows affected.
///
/// # Errors
/// Returns [`ExecError`] for SQL-writing or driver failures.
pub async fn delete(pool: &PgPool, query: &DeleteQuery) -> Result<u64, ExecError> {
    let stmt = Postgres.compile_delete(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let result = q.execute(pool).await?;
    Ok(result.rows_affected())
}

/// Run a `SelectQuery` and return raw `PgRow`s — for tooling that needs to
/// render or inspect rows without statically knowing the row type
/// (e.g. the admin UI).
///
/// # Errors
/// Returns [`ExecError`] for SQL-writing or driver failures.
pub async fn select_rows(pool: &PgPool, query: &SelectQuery) -> Result<Vec<PgRow>, ExecError> {
    let stmt = Postgres.compile_select(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    Ok(q.fetch_all(pool).await?)
}

/// Run a `SelectQuery` and return at most one raw `PgRow`. Used by detail
/// views and PK lookups.
///
/// # Errors
/// Returns [`ExecError`] for SQL-writing or driver failures.
pub async fn select_one_row(
    pool: &PgPool,
    query: &SelectQuery,
) -> Result<Option<PgRow>, ExecError> {
    let stmt = Postgres.compile_select(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    Ok(q.fetch_optional(pool).await?)
}

/// Extension trait that drives a `QuerySet` to a bulk `DELETE`.
///
/// Pulled in via `use rustango::sql::Deleter;`.
pub trait Deleter<T: Model + Send> {
    /// Delete every row matching the queryset's filters. Returns rows affected.
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    fn delete(
        self,
        pool: &PgPool,
    ) -> impl std::future::Future<Output = Result<u64, ExecError>> + Send;
}

impl<T: Model + Send> Deleter<T> for QuerySet<T> {
    async fn delete(self, pool: &PgPool) -> Result<u64, ExecError> {
        let query = self.compile_delete()?;
        delete(pool, &query).await
    }
}

/// Extension trait that drives an `UpdateBuilder` to a bulk `UPDATE`.
///
/// Pulled in via `use rustango::sql::Updater;`.
pub trait Updater<T: Model + Send> {
    /// Compile and execute the update. Returns rows affected.
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    fn execute(
        self,
        pool: &PgPool,
    ) -> impl std::future::Future<Output = Result<u64, ExecError>> + Send;
}

impl<T: Model + Send> Updater<T> for UpdateBuilder<T> {
    async fn execute(self, pool: &PgPool) -> Result<u64, ExecError> {
        let query = self.compile()?;
        update(pool, &query).await
    }
}

/// Match on `SqlValue` and bind to a sqlx query builder. Used twice below for
/// `Query` and `QueryAs`, which don't share a bind trait.
macro_rules! bind_match {
    ($q:expr, $value:expr) => {
        match $value {
            // `None::<String>` produces a typed NULL Postgres accepts in any context.
            SqlValue::Null => $q.bind(None::<String>),
            SqlValue::I32(v) => $q.bind(v),
            SqlValue::I64(v) => $q.bind(v),
            SqlValue::F32(v) => $q.bind(v),
            SqlValue::F64(v) => $q.bind(v),
            SqlValue::Bool(v) => $q.bind(v),
            SqlValue::String(v) => $q.bind(v),
            SqlValue::DateTime(v) => $q.bind(v),
            SqlValue::Date(v) => $q.bind(v),
            SqlValue::Uuid(v) => $q.bind(v),
            SqlValue::Json(_) => unreachable!(
                "`SqlValue::Json` requires the `sqlx/json` feature, not enabled in v0.1"
            ),
            SqlValue::List(_) => {
                unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
            }
        }
    };
}

fn bind_query_as<T>(
    q: QueryAs<'_, sqlx::Postgres, T, PgArguments>,
    value: SqlValue,
) -> QueryAs<'_, sqlx::Postgres, T, PgArguments> {
    bind_match!(q, value)
}

fn bind_query(
    q: Query<'_, sqlx::Postgres, PgArguments>,
    value: SqlValue,
) -> Query<'_, sqlx::Postgres, PgArguments> {
    bind_match!(q, value)
}
