//! [`PoolConn`] and [`SqlExecutor`]: run a request's queries on one
//! pooled connection, on any backend.

use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, TryStreamExt as _};

use super::atomic::{self, Gate};
use super::{Dialect, ExecError, PoolTx};
use crate::core::SqlValue;
use crate::sql::{DeletePlan, Pool};

/// One connection held from a [`Pool`]. Every `_on` query given
/// `&mut conn` runs on it, so a handler pays one checkout, not one per query.
///
/// ```ignore
/// let mut conn = pool.acquire().await?;
/// let posts = Post::objects().fetch_on(&mut conn).await?;
/// let total = Post::objects().count_on(&mut conn).await?;
/// ```
///
/// Session state set on it (`SET …`, MySQL `@vars`, SQLite `PRAGMA`) stays
/// with the connection when it goes back to the pool, so the next borrower
/// gets it: call [`Self::close_on_drop`] after changing any.
///
/// Do not call a `&Pool` method while holding one: that takes a second
/// connection, and `max_connections` such requests deadlock the pool.
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

    /// Close the connection on drop instead of returning it to the pool, so
    /// session state set on it cannot reach the next borrower. sqlx never
    /// resets it, and a reset would also undo an `after_connect` setup.
    pub fn close_on_drop(&mut self) {
        match self {
            #[cfg(feature = "postgres")]
            PoolConn::Postgres(c) => c.close_on_drop(),
            #[cfg(feature = "mysql")]
            PoolConn::Mysql(c) => c.close_on_drop(),
            #[cfg(feature = "sqlite")]
            PoolConn::Sqlite(c) => c.close_on_drop(),
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

/// One result row, in the backend's own row type.
#[doc(hidden)]
pub enum RowRef<'r> {
    #[cfg(feature = "postgres")]
    Postgres(&'r sqlx::postgres::PgRow),
    #[cfg(feature = "mysql")]
    Mysql(&'r sqlx::mysql::MySqlRow),
    #[cfg(feature = "sqlite")]
    Sqlite(&'r sqlx::sqlite::SqliteRow),
}

/// Decode each row as it arrives, so raw rows are never all held.
async fn drain<R, U>(
    mut rows: BoxStream<'_, Result<R, sqlx::Error>>,
    mut decode: impl FnMut(&R) -> Result<U, sqlx::Error>,
) -> Result<Vec<U>, sqlx::Error> {
    let mut out = Vec::new();
    while let Some(row) = rows.try_next().await? {
        out.push(decode(&row)?);
    }
    Ok(out)
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

    /// Runs one statement, decoding each row with `decode`. Boxed, so
    /// callers hold no per-executor type across an `.await` (that broke
    /// `Send` on `&Pool`).
    #[doc(hidden)]
    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e;

    /// Runs one statement and returns the rows it affected.
    #[doc(hidden)]
    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<u64, sqlx::Error>>
    where
        'c: 'e;

    /// Whether [`Self::run_delete_plan`] can run several statements.
    #[doc(hidden)]
    fn runs_plans(&self) -> bool {
        false
    }

    /// Runs a delete plan's statements in one transaction. Only reached
    /// on dialects that split it, so never on a Postgres sqlx executor.
    #[doc(hidden)]
    fn run_delete_plan<'e>(self, plan: DeletePlan) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        let _ = plan;
        let dialect = self.dialect().name();
        Box::pin(async move {
            Err(ExecError::Sql(
                crate::sql::SqlError::OperatorNotSupportedInDialect {
                    op: "multi-statement DELETE",
                    dialect,
                },
            ))
        })
    }
}

mod sealed {
    pub trait Sealed<'c> {}
}

#[cfg(feature = "postgres")]
fn pg_rows<'c: 'e, 'e, 'q: 'e, E, U, F>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
    mut decode: F,
) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    U: Send + 'e,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
{
    let rows = exec.fetch(params.into_iter().fold(sqlx::query(sql), super::bind_query));
    Box::pin(drain(rows, move |r| decode(RowRef::Postgres(r))))
}

#[cfg(feature = "mysql")]
fn my_rows<'c: 'e, 'e, 'q: 'e, E, U, F>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
    mut decode: F,
) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
    U: Send + 'e,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
{
    let rows = exec.fetch(
        params
            .into_iter()
            .fold(sqlx::query(sql), super::bind_query_my),
    );
    Box::pin(drain(rows, move |r| decode(RowRef::Mysql(r))))
}

#[cfg(feature = "sqlite")]
fn sq_rows<'c: 'e, 'e, 'q: 'e, E, U, F>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
    mut decode: F,
) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
    U: Send + 'e,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
{
    let q = params
        .into_iter()
        .fold(sqlx::query(sql), super::bind_query_sqlite);
    let rows = exec.fetch(q);
    Box::pin(drain(rows, move |r| decode(RowRef::Sqlite(r))))
}

#[cfg(feature = "postgres")]
fn pg_exec<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
) -> BoxFuture<'e, Result<u64, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let q = params.into_iter().fold(sqlx::query(sql), super::bind_query);
    let done = exec.execute(q);
    Box::pin(async move { Ok(done.await?.rows_affected()) })
}

#[cfg(feature = "mysql")]
fn my_exec<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
) -> BoxFuture<'e, Result<u64, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
{
    let q = params
        .into_iter()
        .fold(sqlx::query(sql), super::bind_query_my);
    let done = exec.execute(q);
    Box::pin(async move { Ok(done.await?.rows_affected()) })
}

#[cfg(feature = "sqlite")]
fn sq_exec<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: Vec<SqlValue>,
) -> BoxFuture<'e, Result<u64, sqlx::Error>>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    let q = params
        .into_iter()
        .fold(sqlx::query(sql), super::bind_query_sqlite);
    let done = exec.execute(q);
    Box::pin(async move { Ok(done.await?.rows_affected()) })
}

impl<'c> sealed::Sealed<'c> for &'c Pool {}
impl<'c> SqlExecutor<'c> for &'c Pool {
    fn dialect(&self) -> &'static dyn Dialect {
        Pool::dialect(self)
    }

    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => pg_rows(p, sql, params, decode),
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => my_rows(p, sql, params, decode),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => sq_rows(p, sql, params, decode),
        }
    }

    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<u64, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => pg_exec(p, sql, params),
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => my_exec(p, sql, params),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => sq_exec(p, sql, params),
        }
    }

    fn runs_plans(&self) -> bool {
        true
    }

    fn run_delete_plan<'e>(self, plan: DeletePlan) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        Box::pin(super::delete_plan_pool(self, plan))
    }
}

impl<'c> sealed::Sealed<'c> for &'c mut PoolConn {}
impl<'c> SqlExecutor<'c> for &'c mut PoolConn {
    fn dialect(&self) -> &'static dyn Dialect {
        PoolConn::dialect(self)
    }

    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            PoolConn::Postgres(c) => pg_rows(&mut **c, sql, params, decode),
            #[cfg(feature = "mysql")]
            PoolConn::Mysql(c) => my_rows(&mut **c, sql, params, decode),
            #[cfg(feature = "sqlite")]
            PoolConn::Sqlite(c) => sq_rows(&mut **c, sql, params, decode),
        }
    }

    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<u64, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            PoolConn::Postgres(c) => pg_exec(&mut **c, sql, params),
            #[cfg(feature = "mysql")]
            PoolConn::Mysql(c) => my_exec(&mut **c, sql, params),
            #[cfg(feature = "sqlite")]
            PoolConn::Sqlite(c) => sq_exec(&mut **c, sql, params),
        }
    }

    fn runs_plans(&self) -> bool {
        true
    }

    fn run_delete_plan<'e>(self, plan: DeletePlan) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            let mut tx = self.begin().await?;
            let n = super::run_delete_plan_tx(&mut tx, plan).await?;
            tx.commit().await?;
            Ok(n)
        })
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

    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(t) => pg_rows(&mut **t, sql, params, decode),
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(t) => my_rows(&mut **t, sql, params, decode),
            #[cfg(feature = "sqlite")]
            PoolTx::Sqlite(t) => sq_rows(&mut **t, sql, params, decode),
        }
    }

    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<u64, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(t) => pg_exec(&mut **t, sql, params),
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(t) => my_exec(&mut **t, sql, params),
            #[cfg(feature = "sqlite")]
            PoolTx::Sqlite(t) => sq_exec(&mut **t, sql, params),
        }
    }

    fn runs_plans(&self) -> bool {
        true
    }

    fn run_delete_plan<'e>(self, plan: DeletePlan) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        Box::pin(super::run_delete_plan_tx(self, plan))
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

    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
    {
        pg_rows(self, sql, params, decode)
    }

    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: Vec<SqlValue>,
    ) -> BoxFuture<'e, Result<u64, sqlx::Error>>
    where
        'c: 'e,
    {
        pg_exec(self, sql, params)
    }
}

// A `DatabaseTenant` connection, so `fetch_on(t.conn())` works on every backend.
macro_rules! database_conn_executor {
    ($feature:literal, $db:ty, $rows:ident, $exec:ident, $variant:ident, $dialect:path) => {
        #[cfg(all(feature = "tenancy", feature = $feature))]
        impl<'c> sealed::Sealed<'c> for &'c mut crate::tenancy::DatabaseConn<$db> {}
        #[cfg(all(feature = "tenancy", feature = $feature))]
        impl<'c> SqlExecutor<'c> for &'c mut crate::tenancy::DatabaseConn<$db> {
            fn dialect(&self) -> &'static dyn Dialect {
                $dialect
            }

            fn fetch_rows<'e, 'q: 'e, U, F>(
                self,
                sql: &'q str,
                params: Vec<SqlValue>,
                decode: F,
            ) -> BoxFuture<'e, Result<Vec<U>, sqlx::Error>>
            where
                'c: 'e,
                U: Send + 'e,
                F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
            {
                $rows(&mut ***self, sql, params, decode)
            }

            fn execute_rows<'e, 'q: 'e>(
                self,
                sql: &'q str,
                params: Vec<SqlValue>,
            ) -> BoxFuture<'e, Result<u64, sqlx::Error>>
            where
                'c: 'e,
            {
                $exec(&mut ***self, sql, params)
            }

            fn run_delete_plan<'e>(self, plan: DeletePlan) -> BoxFuture<'e, Result<u64, ExecError>>
            where
                'c: 'e,
            {
                Box::pin(async move {
                    use sqlx::Connection as _;
                    let mut tx = PoolTx::$variant((***self).begin().await?);
                    let n = super::run_delete_plan_tx(&mut tx, plan).await?;
                    tx.commit().await?;
                    Ok(n)
                })
            }
        }
    };
}

database_conn_executor!(
    "mysql",
    sqlx::MySql,
    my_rows,
    my_exec,
    Mysql,
    crate::sql::mysql::DIALECT
);
database_conn_executor!(
    "sqlite",
    sqlx::Sqlite,
    sq_rows,
    sq_exec,
    Sqlite,
    crate::sql::sqlite::DIALECT
);
