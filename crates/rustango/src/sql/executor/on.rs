//! [`PoolConn`] and [`SqlExecutor`]: run a request's queries on one
//! pooled connection, on any backend (#2675).

use futures_util::future::BoxFuture;

use super::atomic::{self, Gate};
use super::{Dialect, ExecError, PoolTx};
use crate::core::SqlValue;
use crate::sql::Pool;

/// One connection held from a [`Pool`]. Every `_on` query given
/// `&mut conn` runs on it, so a handler pays one checkout, not one per query.
///
/// ```ignore
/// let mut conn = pool.acquire().await?;
/// let posts = Post::objects().fetch_on(&mut conn).await?;
/// let total = Post::objects().count_on(&mut conn).await?;
/// ```
///
/// Session state set on it (`SET …`) stays with the connection when it
/// goes back to the pool.
#[non_exhaustive]
pub enum PoolConn {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::pool::PoolConnection<sqlx::Postgres>),
    #[cfg(feature = "mysql")]
    Mysql(sqlx::pool::PoolConnection<sqlx::MySql>),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::pool::PoolConnection<sqlx::Sqlite>),
}

impl Pool {
    /// Take one connection to run several queries on; see [`PoolConn`].
    ///
    /// # Errors
    /// The driver's acquire error, e.g. a pool timeout.
    pub async fn acquire(&self) -> Result<PoolConn, ExecError> {
        Ok(match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => PoolConn::Postgres(p.acquire().await?),
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => PoolConn::Mysql(p.acquire().await?),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => PoolConn::Sqlite(p.acquire().await?),
        })
    }
}

impl PoolConn {
    /// The dialect of this connection's backend.
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        match self {
            #[cfg(feature = "postgres")]
            PoolConn::Postgres(_) => crate::sql::postgres::DIALECT,
            #[cfg(feature = "mysql")]
            PoolConn::Mysql(_) => crate::sql::mysql::DIALECT,
            #[cfg(feature = "sqlite")]
            PoolConn::Sqlite(_) => crate::sql::sqlite::DIALECT,
        }
    }

    /// Open a transaction on this connection.
    ///
    /// # Errors
    /// Driver errors from `BEGIN`.
    pub async fn begin(&mut self) -> Result<PoolTx<'_>, ExecError> {
        use sqlx::Connection as _;
        Ok(match self {
            #[cfg(feature = "postgres")]
            PoolConn::Postgres(c) => PoolTx::Postgres((**c).begin().await?),
            #[cfg(feature = "mysql")]
            PoolConn::Mysql(c) => PoolTx::Mysql((**c).begin().await?),
            #[cfg(feature = "sqlite")]
            PoolConn::Sqlite(c) => PoolTx::Sqlite((**c).begin().await?),
        })
    }
}

/// Rows of one statement, in the backend's own row type.
#[doc(hidden)]
pub enum Rows {
    #[cfg(feature = "postgres")]
    Postgres(Vec<sqlx::postgres::PgRow>),
    #[cfg(feature = "mysql")]
    Mysql(Vec<sqlx::mysql::MySqlRow>),
    #[cfg(feature = "sqlite")]
    Sqlite(Vec<sqlx::sqlite::SqliteRow>),
}

/// Refuses a statement on an `atomic` transaction the server ended.
#[doc(hidden)]
pub struct Guard(Gate);

impl Guard {
    pub(crate) fn check<T>(&self, r: Result<T, ExecError>) -> Result<T, ExecError> {
        self.0.check(r)
    }
}

/// What the `_on` query methods run on: `&mut PoolConn`, `&mut PoolTx`,
/// `&Pool`, or any Postgres sqlx executor (`&PgPool`, `&mut PgConnection`).
/// Sealed.
pub trait SqlExecutor<'c>: sealed::Sealed<'c> + Send + Sized {
    #[doc(hidden)]
    fn dialect(&self) -> &'static dyn Dialect;

    #[doc(hidden)]
    fn guard(&self) -> Result<Guard, ExecError> {
        Ok(Guard(Gate::OPEN))
    }

    /// Runs one statement. Boxed, so callers hold no per-executor type
    /// across an `.await` (that broke `Send` on `&Pool`).
    #[doc(hidden)]
    fn fetch_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
    where
        'c: 'e;
}

mod sealed {
    pub trait Sealed<'c> {}
}

#[cfg(feature = "postgres")]
fn pg_rows<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let fut = exec.fetch_all(params.into_iter().fold(sqlx::query(sql), super::bind_query));
    Box::pin(async move { fut.await.map(Rows::Postgres) })
}

#[cfg(feature = "mysql")]
fn my_rows<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
{
    let fut = exec.fetch_all(params.into_iter().fold(sqlx::query(sql), super::bind_query_my));
    Box::pin(async move { fut.await.map(Rows::Mysql) })
}

#[cfg(feature = "sqlite")]
fn sq_rows<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    let q = params
        .into_iter()
        .fold(sqlx::query(sql), super::bind_query_sqlite);
    let fut = exec.fetch_all(q);
    Box::pin(async move { fut.await.map(Rows::Sqlite) })
}

impl<'c> sealed::Sealed<'c> for &'c Pool {}
impl<'c> SqlExecutor<'c> for &'c Pool {
    fn dialect(&self) -> &'static dyn Dialect {
        Pool::dialect(self)
    }

    fn fetch_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => pg_rows(p, sql, params),
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => my_rows(p, sql, params),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => sq_rows(p, sql, params),
        }
    }
}

impl<'c> sealed::Sealed<'c> for &'c mut PoolConn {}
impl<'c> SqlExecutor<'c> for &'c mut PoolConn {
    fn dialect(&self) -> &'static dyn Dialect {
        PoolConn::dialect(self)
    }

    fn fetch_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            PoolConn::Postgres(c) => pg_rows(&mut **c, sql, params),
            #[cfg(feature = "mysql")]
            PoolConn::Mysql(c) => my_rows(&mut **c, sql, params),
            #[cfg(feature = "sqlite")]
            PoolConn::Sqlite(c) => sq_rows(&mut **c, sql, params),
        }
    }
}

impl<'c> sealed::Sealed<'c> for &'c mut PoolTx<'_> {}
impl<'c> SqlExecutor<'c> for &'c mut PoolTx<'_> {
    fn dialect(&self) -> &'static dyn Dialect {
        PoolTx::dialect(self)
    }

    fn guard(&self) -> Result<Guard, ExecError> {
        atomic::gate(self).map(Guard)
    }

    fn fetch_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(t) => pg_rows(&mut **t, sql, params),
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(t) => my_rows(&mut **t, sql, params),
            #[cfg(feature = "sqlite")]
            PoolTx::Sqlite(t) => sq_rows(&mut **t, sql, params),
        }
    }
}

// Any Postgres sqlx executor, so `fetch_on(t.conn())` keeps compiling.
#[cfg(feature = "postgres")]
impl<'c, E: sqlx::Executor<'c, Database = sqlx::Postgres>> sealed::Sealed<'c> for E {}
#[cfg(feature = "postgres")]
impl<'c, E: sqlx::Executor<'c, Database = sqlx::Postgres>> SqlExecutor<'c> for E {
    fn dialect(&self) -> &'static dyn Dialect {
        crate::sql::postgres::DIALECT
    }

    fn fetch_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<Rows, sqlx::Error>>
    where
        'c: 'e,
    {
        pg_rows(self, sql, params)
    }
}
