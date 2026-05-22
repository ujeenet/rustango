//! Pure projection — Django `.values()` / `.values_list()` (issue #22).
//!
//! Extracted from `executor/mod.rs` as part of #116 step 3. Contains:
//!
//! - Per-dialect `cell_to_sqlvalue` helpers (PG / MySQL / SQLite).
//! - `fetch_values_dict` / `fetch_values_list` / `fetch_values_flat`
//!   pool-level entry points.
//! - `MaybePgScalar` / `MaybeMyScalar` / `MaybeSqliteScalar` trait
//!   gates for `values_list_flat::<U>(...)`.
//! - `impl ValuesQuerySet` / `impl ValuesListQuerySet` /
//!   `impl ValuesFlatQuerySet` bridge methods that let callers chain
//!   `.fetch(&pool)`.

#[cfg(feature = "postgres")]
use sqlx::postgres::{PgArguments, PgRow};
#[cfg(feature = "postgres")]
use sqlx::query::Query;

use super::ExecError;
use crate::core::{SelectQuery, SqlValue};
use crate::sql::Pool;

#[cfg(feature = "postgres")]
use super::{bind_match, bind_query};
#[cfg(feature = "mysql")]
use super::{bind_match_mysql, bind_query_my};
#[cfg(feature = "sqlite")]
use super::{bind_match_sqlite, bind_query_sqlite};

/// Decode one per-dialect raw `SqlValue` from the i-th column of a row.
/// Same probe order as the aggregate-row decoder so the two paths agree
/// on how mixed types come back: scalars first, then jsonb / arrays
/// (PG only). NULLs (or unrecognized types) fall through to
/// [`SqlValue::Null`].
#[cfg(feature = "postgres")]
fn pg_cell_to_sqlvalue(row: &PgRow, i: usize) -> SqlValue {
    use sqlx::Row as _;
    if let Ok(v) = row.try_get::<i64, _>(i) {
        SqlValue::I64(v)
    } else if let Ok(v) = row.try_get::<i32, _>(i) {
        SqlValue::I32(v)
    } else if let Ok(v) = row.try_get::<f64, _>(i) {
        SqlValue::F64(v)
    } else if let Ok(v) = row.try_get::<bool, _>(i) {
        SqlValue::Bool(v)
    } else if let Ok(v) = row.try_get::<String, _>(i) {
        SqlValue::String(v)
    } else if let Ok(v) = row.try_get::<serde_json::Value, _>(i) {
        SqlValue::Json(v)
    } else {
        SqlValue::Null
    }
}

#[cfg(feature = "mysql")]
fn my_cell_to_sqlvalue(row: &sqlx::mysql::MySqlRow, i: usize) -> SqlValue {
    use sqlx::Row as _;
    if let Ok(v) = row.try_get::<i64, _>(i) {
        SqlValue::I64(v)
    } else if let Ok(v) = row.try_get::<i32, _>(i) {
        SqlValue::I32(v)
    } else if let Ok(v) = row.try_get::<f64, _>(i) {
        SqlValue::F64(v)
    } else if let Ok(v) = row.try_get::<bool, _>(i) {
        SqlValue::Bool(v)
    } else if let Ok(v) = row.try_get::<String, _>(i) {
        SqlValue::String(v)
    } else {
        SqlValue::Null
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_cell_to_sqlvalue(row: &sqlx::sqlite::SqliteRow, i: usize) -> SqlValue {
    use sqlx::Row as _;
    if let Ok(v) = row.try_get::<i64, _>(i) {
        SqlValue::I64(v)
    } else if let Ok(v) = row.try_get::<i32, _>(i) {
        SqlValue::I32(v)
    } else if let Ok(v) = row.try_get::<f64, _>(i) {
        SqlValue::F64(v)
    } else if let Ok(v) = row.try_get::<bool, _>(i) {
        SqlValue::Bool(v)
    } else if let Ok(v) = row.try_get::<String, _>(i) {
        SqlValue::String(v)
    } else {
        SqlValue::Null
    }
}

/// Execute a [`SelectQuery`] (with `projection` set) and return each
/// row as a `HashMap<String, SqlValue>` keyed by column name.
/// Backs [`crate::query::ValuesQuerySet::fetch`]. Issue #22.
///
/// # Errors
/// SQL compilation or driver failure.
pub async fn fetch_values_dict(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(col.name().to_owned(), pg_cell_to_sqlvalue(row, i));
                }
                out.push(map);
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(col.name().to_owned(), my_cell_to_sqlvalue(row, i));
                }
                out.push(map);
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Column as _;
                use sqlx::Row as _;
                let mut map = std::collections::HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    map.insert(col.name().to_owned(), sqlite_cell_to_sqlvalue(row, i));
                }
                out.push(map);
            }
            Ok(out)
        }
    }
}

/// Execute a [`SelectQuery`] (with `projection` set) and return each
/// row as a `Vec<SqlValue>` ordered to match the projection's column
/// list. Backs [`crate::query::ValuesListQuerySet::fetch`].
/// Issue #22.
///
/// # Errors
/// SQL compilation or driver failure.
pub async fn fetch_values_list(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<Vec<SqlValue>>, ExecError> {
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Row as _;
                let n = row.columns().len();
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push(pg_cell_to_sqlvalue(row, i));
                }
                out.push(v);
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Row as _;
                let n = row.columns().len();
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push(my_cell_to_sqlvalue(row, i));
                }
                out.push(v);
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                use sqlx::Row as _;
                let n = row.columns().len();
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push(sqlite_cell_to_sqlvalue(row, i));
                }
                out.push(v);
            }
            Ok(out)
        }
    }
}

/// Trait gate for the `.values_list_flat::<U>(...)` typed-scalar path —
/// PG arm. Same shape as [`super::MaybePgFromRow`]: when the `postgres`
/// feature is on, this is `Decode + Type<Postgres>`; otherwise an
/// empty blanket-impl so non-PG builds compile.
#[cfg(feature = "postgres")]
pub trait MaybePgScalar:
    for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>
{
}
#[cfg(feature = "postgres")]
impl<T> MaybePgScalar for T where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>
{
}
#[cfg(not(feature = "postgres"))]
pub trait MaybePgScalar {}
#[cfg(not(feature = "postgres"))]
impl<T> MaybePgScalar for T {}

#[cfg(feature = "mysql")]
pub trait MaybeMyScalar: for<'r> sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql> {}
#[cfg(feature = "mysql")]
impl<T> MaybeMyScalar for T where T: for<'r> sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql> {}
#[cfg(not(feature = "mysql"))]
pub trait MaybeMyScalar {}
#[cfg(not(feature = "mysql"))]
impl<T> MaybeMyScalar for T {}

#[cfg(feature = "sqlite")]
pub trait MaybeSqliteScalar:
    for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>
{
}
#[cfg(feature = "sqlite")]
impl<T> MaybeSqliteScalar for T where
    T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>
{
}
#[cfg(not(feature = "sqlite"))]
pub trait MaybeSqliteScalar {}
#[cfg(not(feature = "sqlite"))]
impl<T> MaybeSqliteScalar for T {}

/// Execute a single-column [`SelectQuery`] and decode each row's only
/// cell into `U`. Backs [`crate::query::ValuesFlatQuerySet::fetch`].
/// Issue #22 — Django's `.values_list('col', flat=True)`.
///
/// # Errors
/// SQL compilation or driver failure, including a decode error if `U`
/// doesn't match the column's SQL type on the live database.
pub async fn fetch_values_flat<U>(pool: &Pool, query: &SelectQuery) -> Result<Vec<U>, ExecError>
where
    U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
{
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: sqlx::query::QueryScalar<'_, sqlx::Postgres, U, PgArguments> =
                sqlx::query_scalar(&stmt.sql);
            for v in stmt.params {
                q = bind_query_scalar_pg(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryScalar<'_, sqlx::MySql, U, sqlx::mysql::MySqlArguments> =
                sqlx::query_scalar(&stmt.sql);
            for v in stmt.params {
                q = bind_query_scalar_my(q, v);
            }
            Ok(q.fetch_all(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryScalar<
                '_,
                sqlx::Sqlite,
                U,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_scalar(&stmt.sql);
            for v in stmt.params {
                q = bind_query_scalar_sqlite(q, v);
            }
            Ok(q.fetch_all(sq).await?)
        }
    }
}

#[cfg(feature = "postgres")]
fn bind_query_scalar_pg<U>(
    q: sqlx::query::QueryScalar<'_, sqlx::Postgres, U, PgArguments>,
    value: SqlValue,
) -> sqlx::query::QueryScalar<'_, sqlx::Postgres, U, PgArguments> {
    bind_match!(q, value)
}

#[cfg(feature = "mysql")]
fn bind_query_scalar_my<U>(
    q: sqlx::query::QueryScalar<'_, sqlx::MySql, U, sqlx::mysql::MySqlArguments>,
    value: SqlValue,
) -> sqlx::query::QueryScalar<'_, sqlx::MySql, U, sqlx::mysql::MySqlArguments> {
    bind_match_mysql!(q, value)
}

#[cfg(feature = "sqlite")]
fn bind_query_scalar_sqlite<'a, U>(
    q: sqlx::query::QueryScalar<'a, sqlx::Sqlite, U, sqlx::sqlite::SqliteArguments<'a>>,
    value: SqlValue,
) -> sqlx::query::QueryScalar<'a, sqlx::Sqlite, U, sqlx::sqlite::SqliteArguments<'a>> {
    bind_match_sqlite!(q, value)
}

// Bridge methods on the values builders so callers chain `.fetch(&pool)`.

impl<T: crate::core::Model> crate::query::ValuesQuerySet<T> {
    /// Execute the projection and return rows as `Vec<HashMap<String, SqlValue>>`.
    ///
    /// # Errors
    /// - [`ExecError::Query`] for SQL compilation failures (typo'd
    ///   column, etc.).
    /// - [`ExecError::Sqlx`] for driver / network / decode failures.
    pub async fn fetch(
        self,
        pool: &Pool,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
        let q = self.compile()?;
        fetch_values_dict(pool, &q).await
    }
}

impl<T: crate::core::Model> crate::query::ValuesListQuerySet<T> {
    /// Execute the projection and return rows as `Vec<Vec<SqlValue>>`.
    ///
    /// # Errors
    /// As [`crate::query::ValuesQuerySet::fetch`].
    pub async fn fetch(self, pool: &Pool) -> Result<Vec<Vec<SqlValue>>, ExecError> {
        let q = self.compile()?;
        fetch_values_list(pool, &q).await
    }
}

impl<T: crate::core::Model> crate::query::ValuesFlatQuerySet<T> {
    /// Execute the single-column projection and decode each row's cell
    /// into `U`.
    ///
    /// # Errors
    /// As [`crate::query::ValuesQuerySet::fetch`], plus per-cell
    /// type-mismatch errors if `U` doesn't match the column's SQL type.
    pub async fn fetch<U>(self, pool: &Pool) -> Result<Vec<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        let q = self.compile()?;
        fetch_values_flat::<U>(pool, &q).await
    }
}
