//! [`PoolConn`] and [`SqlExecutor`]: run a request's queries on one
//! pooled connection, on any backend.

use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, TryStreamExt as _};

use super::atomic::{self, Gate, Stmt};
use super::{Dialect, ExecError, InsertReturningPool, PoolTx, TxConn};
use crate::core::{BulkInsertQuery, InsertQuery};
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
///
/// Reach the driver connection through [`Self::conn`]. It remembers its
/// pool, so audit rows in [`Self::begin`] keep a tenant-bound source.
pub struct PoolConn {
    conn: PooledConn,
    pool: crate::sql::PoolId,
}

/// The driver-typed connection inside a [`PoolConn`].
#[non_exhaustive]
pub enum PooledConn {
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
        super::refuse_in_migration_step()?;
        let conn = match self {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => PooledConn::Postgres(p.acquire().await?),
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => PooledConn::Mysql(p.acquire().await?),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => PooledConn::Sqlite(p.acquire().await?),
        };
        Ok(PoolConn {
            conn,
            pool: crate::sql::PoolId::of(self),
        })
    }
}

impl PoolConn {
    /// The driver-typed connection, to run sqlx queries on.
    pub fn conn(&mut self) -> &mut PooledConn {
        &mut self.conn
    }

    /// The dialect of this connection's backend.
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        match &self.conn {
            #[cfg(feature = "postgres")]
            PooledConn::Postgres(_) => crate::sql::postgres::DIALECT,
            #[cfg(feature = "mysql")]
            PooledConn::Mysql(_) => crate::sql::mysql::DIALECT,
            #[cfg(feature = "sqlite")]
            PooledConn::Sqlite(_) => crate::sql::sqlite::DIALECT,
        }
    }

    /// Close the connection on drop instead of returning it to the pool, so
    /// session state set on it cannot reach the next borrower. sqlx never
    /// resets it, and a reset would also undo an `after_connect` setup.
    pub fn close_on_drop(&mut self) {
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            PooledConn::Postgres(c) => c.close_on_drop(),
            #[cfg(feature = "mysql")]
            PooledConn::Mysql(c) => c.close_on_drop(),
            #[cfg(feature = "sqlite")]
            PooledConn::Sqlite(c) => c.close_on_drop(),
        }
    }

    /// Open a transaction on this connection.
    ///
    /// # Errors
    /// Driver errors from `BEGIN`.
    pub async fn begin(&mut self) -> Result<PoolTx<'_>, ExecError> {
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            PooledConn::Postgres(c) => PoolTx::begin_on(&mut **c, &self.pool).await,
            #[cfg(feature = "mysql")]
            PooledConn::Mysql(c) => PoolTx::begin_on(&mut **c, &self.pool).await,
            #[cfg(feature = "sqlite")]
            PooledConn::Sqlite(c) => PoolTx::begin_on(&mut **c, &self.pool).await,
        }
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
) -> Result<Vec<U>, ExecError> {
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
        params: crate::sql::BindParams,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e;

    /// Runs one statement and returns the rows it affected.
    #[doc(hidden)]
    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: crate::sql::BindParams,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e;

    /// `page`, then `count` when the page is empty, for a page past the
    /// last row. `None` where the executor cannot run a second statement.
    #[doc(hidden)]
    fn fetch_rows_or_count<'e, U, F>(
        self,
        page: crate::sql::CompiledStatement,
        count: Option<crate::sql::CompiledStatement>,
        decode: F,
    ) -> BoxFuture<'e, Result<(Vec<U>, Option<i64>), ExecError>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
        Self: 'e,
    {
        let _ = count;
        Box::pin(async move {
            let rows = super::run_on(self, &page.sql, page.params, decode).await?;
            Ok((rows, None))
        })
    }

    /// Runs `stmts` all or none; returns the rows they affected. One
    /// statement runs alone; several need a transaction this executor can
    /// open, so a raw sqlx executor refuses them.
    #[doc(hidden)]
    fn run_batches<'e>(
        self,
        mut stmts: Vec<crate::sql::CompiledStatement>,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        let dialect = self.dialect().name();
        Box::pin(async move {
            match stmts.len() {
                0 => Ok(0),
                1 => {
                    let stmt = stmts.remove(0);
                    super::write_on(self, &stmt.sql, stmt.params).await
                }
                _ => Err(ExecError::Sql(
                    crate::sql::SqlError::OperatorNotSupportedInDialect {
                        op: "a multi-statement write on a raw sqlx executor",
                        dialect,
                    },
                )),
            }
        })
    }

    /// Whether [`Self::run_delete_plan`] can run several statements.
    #[doc(hidden)]
    fn runs_plans(&self) -> bool {
        false
    }

    /// INSERT … RETURNING one row (MySQL: the new auto id) on one connection.
    #[doc(hidden)]
    fn insert_returning<'e>(
        self,
        query: InsertQuery,
    ) -> BoxFuture<'e, Result<InsertReturningPool, ExecError>>
    where
        'c: 'e,
        Self: 'e;

    /// Bulk INSERT in one transaction, returning its `RETURNING` rows.
    #[doc(hidden)]
    fn bulk_insert<'e>(
        self,
        query: BulkInsertQuery,
    ) -> BoxFuture<'e, Result<Vec<InsertReturningPool>, ExecError>>
    where
        'c: 'e,
        Self: 'e;

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
    pub trait Reusable {}
}

/// What the multi-statement `_on` forms run on: `&Pool`, `&mut PoolConn`
/// or `&mut PoolTx`, each statement in turn on the same one. Sealed.
pub trait ReusableExecutor: sealed::Reusable + Send {
    /// The executor one statement takes.
    #[doc(hidden)]
    type Exec<'a>: SqlExecutor<'a>
    where
        Self: 'a;

    /// Lend this executor to one statement.
    #[doc(hidden)]
    fn again(&mut self) -> Self::Exec<'_>;
}

impl sealed::Reusable for &Pool {}
impl ReusableExecutor for &Pool {
    type Exec<'a>
        = &'a Pool
    where
        Self: 'a;

    fn again(&mut self) -> &Pool {
        self
    }
}

impl sealed::Reusable for &mut PoolConn {}
impl ReusableExecutor for &mut PoolConn {
    type Exec<'a>
        = &'a mut PoolConn
    where
        Self: 'a;

    fn again(&mut self) -> &mut PoolConn {
        self
    }
}

impl<'t> sealed::Reusable for &mut PoolTx<'t> {}
impl<'t> ReusableExecutor for &mut PoolTx<'t> {
    type Exec<'a>
        = &'a mut PoolTx<'t>
    where
        Self: 'a;

    fn again(&mut self) -> &mut PoolTx<'t> {
        self
    }
}

#[cfg(feature = "postgres")]
fn pg_rows<'c: 'e, 'e, 'q: 'e, E, U, F>(
    exec: E,
    sql: &'q str,
    params: crate::sql::BindParams,
    mut decode: F,
) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    U: Send + 'e,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
{
    let q = match super::bind_pg(sqlx::query(sql), params) {
        Ok(q) => q,
        Err(e) => return Box::pin(std::future::ready(Err(e.into()))),
    };
    Box::pin(drain(exec.fetch(q), move |r| decode(RowRef::Postgres(r))))
}

#[cfg(feature = "mysql")]
fn my_rows<'c: 'e, 'e, 'q: 'e, E, U, F>(
    exec: E,
    sql: &'q str,
    params: crate::sql::BindParams,
    mut decode: F,
) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
    U: Send + 'e,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
{
    let q = match super::bind_my(sqlx::query(sql), params) {
        Ok(q) => q,
        Err(e) => return Box::pin(std::future::ready(Err(e.into()))),
    };
    Box::pin(drain(exec.fetch(q), move |r| decode(RowRef::Mysql(r))))
}

#[cfg(feature = "sqlite")]
fn sq_rows<'c: 'e, 'e, 'q: 'e, E, U, F>(
    exec: E,
    sql: &'q str,
    params: crate::sql::BindParams,
    mut decode: F,
) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
    U: Send + 'e,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
{
    let q = match super::bind_sqlite(sqlx::query(sql), params) {
        Ok(q) => q,
        Err(e) => return Box::pin(std::future::ready(Err(e.into()))),
    };
    Box::pin(drain(exec.fetch(q), move |r| decode(RowRef::Sqlite(r))))
}

#[cfg(feature = "postgres")]
fn pg_exec<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: crate::sql::BindParams,
) -> BoxFuture<'e, Result<u64, ExecError>>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let q = match super::bind_pg(sqlx::query(sql), params) {
        Ok(q) => q,
        Err(e) => return Box::pin(std::future::ready(Err(e.into()))),
    };
    crate::test_assertions::query_counter::bump();
    let done = exec.execute(q);
    Box::pin(async move { Ok(done.await?.rows_affected()) })
}

#[cfg(feature = "mysql")]
fn my_exec<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: crate::sql::BindParams,
) -> BoxFuture<'e, Result<u64, ExecError>>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
{
    let q = match super::bind_my(sqlx::query(sql), params) {
        Ok(q) => q,
        Err(e) => return Box::pin(std::future::ready(Err(e.into()))),
    };
    crate::test_assertions::query_counter::bump();
    let done = exec.execute(q);
    Box::pin(async move { Ok(done.await?.rows_affected()) })
}

#[cfg(feature = "sqlite")]
fn sq_exec<'c: 'e, 'e, 'q: 'e, E>(
    exec: E,
    sql: &'q str,
    params: crate::sql::BindParams,
) -> BoxFuture<'e, Result<u64, ExecError>>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    let q = match super::bind_sqlite(sqlx::query(sql), params) {
        Ok(q) => q,
        Err(e) => return Box::pin(std::future::ready(Err(e.into()))),
    };
    crate::test_assertions::query_counter::bump();
    let done = exec.execute(q);
    Box::pin(async move { Ok(done.await?.rows_affected()) })
}

/// [`SqlExecutor::fetch_rows_or_count`] for an executor `$again` can reuse.
macro_rules! fetch_rows_or_count {
    ($me:ident => $again:expr) => {
        fn fetch_rows_or_count<'e, U, F>(
            self,
            page: crate::sql::CompiledStatement,
            count: Option<crate::sql::CompiledStatement>,
            decode: F,
        ) -> BoxFuture<'e, Result<(Vec<U>, Option<i64>), ExecError>>
        where
            'c: 'e,
            U: Send + 'e,
            F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
            Self: 'e,
        {
            #[allow(unused_mut)]
            let mut $me = self;
            Box::pin(async move {
                let rows = super::run_on($again, &page.sql, page.params, decode).await?;
                let total = match count.filter(|_| rows.is_empty()) {
                    Some(c) => Some(super::count_stmt_on($again, c).await?),
                    None => None,
                };
                Ok((rows, total))
            })
        }
    };
}

/// Run one `$stmt` on `$pool`: `$joined`, with `$tx` the open `atomic`
/// block's transaction, if there is one, else `$alone`. Every `&Pool`
/// statement joins a block through here, so all get the same savepoint,
/// gate and one-statement-at-a-time rules ([`atomic::Joined`]).
macro_rules! on_pool {
    ($pool:expr, $stmt:expr, |$tx:ident| $joined:expr, $alone:expr $(,)?) => {{
        $crate::sql::executor::refuse_in_migration_step()?;
        match $crate::sql::executor::atomic::join($pool, $stmt).await {
            Some(joined) => {
                let mut joined = joined?;
                let r = {
                    let $tx: &mut $crate::sql::PoolTx<'static> = &mut joined;
                    $joined
                };
                joined.finish(r).await
            }
            None => $alone,
        }
    }};
}
pub(super) use on_pool;

/// Rows of one `stmt` on `pool`, on its open `atomic` block if any.
pub(super) fn pool_rows<'e, U, F>(
    pool: &'e Pool,
    stmt: Stmt,
    sql: &'e str,
    params: crate::sql::BindParams,
    decode: F,
) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
where
    U: Send + 'e,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
{
    Box::pin(async move {
        on_pool!(
            pool,
            stmt,
            |tx| tx.fetch_rows(sql, params, decode).await,
            match pool {
                #[cfg(feature = "postgres")]
                Pool::Postgres(p) => pg_rows(p, sql, params, decode).await,
                #[cfg(feature = "mysql")]
                Pool::Mysql(p) => my_rows(p, sql, params, decode).await,
                #[cfg(feature = "sqlite")]
                Pool::Sqlite(p) => sq_rows(p, sql, params, decode).await,
            }
        )
    })
}

/// Rows affected by one `stmt` on `pool`, on its open `atomic` block if any.
pub(super) async fn pool_exec(
    pool: &Pool,
    stmt: Stmt,
    sql: &str,
    params: crate::sql::BindParams,
) -> Result<u64, ExecError> {
    on_pool!(
        pool,
        stmt,
        |tx| super::execute_tx(tx, sql, params).await,
        super::execute_pool(pool, sql, params).await
    )
}

impl<'c> sealed::Sealed<'c> for &'c Pool {}
impl<'c> SqlExecutor<'c> for &'c Pool {
    fn dialect(&self) -> &'static dyn Dialect {
        Pool::dialect(self)
    }

    fetch_rows_or_count!(me => me);

    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: crate::sql::BindParams,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
    {
        pool_rows(self, Stmt::Plain, sql, params, decode)
    }

    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: crate::sql::BindParams,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        Box::pin(pool_exec(self, Stmt::Write, sql, params))
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

    fn run_batches<'e>(
        self,
        stmts: Vec<crate::sql::CompiledStatement>,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(super::execute_all_or_none(self, stmts))
    }

    fn insert_returning<'e>(
        self,
        query: InsertQuery,
    ) -> BoxFuture<'e, Result<InsertReturningPool, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(
            async move { super::insert_returning_cow(self, std::borrow::Cow::Owned(query)).await },
        )
    }

    fn bulk_insert<'e>(
        self,
        query: BulkInsertQuery,
    ) -> BoxFuture<'e, Result<Vec<InsertReturningPool>, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(async move {
            let mut tx = write_tx(self).await?;
            let rows = super::bulk_insert_rows_tx(&mut tx, std::borrow::Cow::Owned(query)).await?;
            tx.commit().await?;
            Ok(rows)
        })
    }
}

/// The transaction a multi-statement ORM write runs in: its own, or a
/// savepoint of the open [`atomic`](super::atomic()) block on the same pool.
#[doc(hidden)]
pub struct WriteTx(WriteTxConn);

enum WriteTxConn {
    Own(PoolTx<'static>),
    Joined(atomic::Joined),
}

/// Begin a write on `pool`: a savepoint inside an [`atomic`](super::atomic())
/// block on it, else [`write_transaction`](super::write_transaction).
/// Finish with [`WriteTx::commit`].
///
/// # Errors
/// Driver errors from `BEGIN` / `SAVEPOINT`; [`ExecError::NestedAtomic`]
/// while the block's guard is held.
#[doc(hidden)]
pub async fn write_tx(pool: &Pool) -> Result<WriteTx, ExecError> {
    match atomic::join(pool, Stmt::Write).await {
        Some(w) => Ok(WriteTx(WriteTxConn::Joined(w?))),
        None => Ok(WriteTx(WriteTxConn::Own(
            super::write_transaction(pool).await?,
        ))),
    }
}

impl WriteTx {
    /// Commit, or release the block's savepoint (the block commits it).
    ///
    /// # Errors
    /// Driver errors from `COMMIT` / `RELEASE SAVEPOINT`.
    pub async fn commit(self) -> Result<(), ExecError> {
        match self.0 {
            WriteTxConn::Own(tx) => Ok(tx.commit().await?),
            WriteTxConn::Joined(w) => w.release().await,
        }
    }

    /// Roll back, or undo the block's savepoint on its next use.
    ///
    /// # Errors
    /// Driver errors from `ROLLBACK`.
    pub async fn rollback(self) -> Result<(), ExecError> {
        match self.0 {
            WriteTxConn::Own(tx) => Ok(tx.rollback().await?),
            WriteTxConn::Joined(w) => {
                drop(w);
                Ok(())
            }
        }
    }
}

/// A transaction of its own, such as a registry claim's.
impl From<PoolTx<'static>> for WriteTx {
    fn from(tx: PoolTx<'static>) -> Self {
        Self(WriteTxConn::Own(tx))
    }
}

impl std::ops::Deref for WriteTx {
    type Target = PoolTx<'static>;
    fn deref(&self) -> &PoolTx<'static> {
        match &self.0 {
            WriteTxConn::Own(tx) => tx,
            WriteTxConn::Joined(w) => w,
        }
    }
}

impl std::ops::DerefMut for WriteTx {
    fn deref_mut(&mut self) -> &mut PoolTx<'static> {
        match &mut self.0 {
            WriteTxConn::Own(tx) => tx,
            WriteTxConn::Joined(w) => w,
        }
    }
}

impl<'c> sealed::Sealed<'c> for &'c mut PoolConn {}
impl<'c> SqlExecutor<'c> for &'c mut PoolConn {
    fn dialect(&self) -> &'static dyn Dialect {
        PoolConn::dialect(self)
    }

    fetch_rows_or_count!(me => &mut *me);

    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: crate::sql::BindParams,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
    {
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            PooledConn::Postgres(c) => pg_rows(&mut **c, sql, params, decode),
            #[cfg(feature = "mysql")]
            PooledConn::Mysql(c) => my_rows(&mut **c, sql, params, decode),
            #[cfg(feature = "sqlite")]
            PooledConn::Sqlite(c) => sq_rows(&mut **c, sql, params, decode),
        }
    }

    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: crate::sql::BindParams,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            PooledConn::Postgres(c) => pg_exec(&mut **c, sql, params),
            #[cfg(feature = "mysql")]
            PooledConn::Mysql(c) => my_exec(&mut **c, sql, params),
            #[cfg(feature = "sqlite")]
            PooledConn::Sqlite(c) => sq_exec(&mut **c, sql, params),
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

    fn run_batches<'e>(
        self,
        stmts: Vec<crate::sql::CompiledStatement>,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(async move {
            if let [stmt] = stmts.as_slice() {
                return super::write_on(self, &stmt.sql, stmt.params.clone()).await;
            }
            let mut tx = self.begin().await?;
            let n = super::execute_batches_tx(&mut tx, stmts).await?;
            tx.commit().await?;
            Ok(n)
        })
    }

    fn insert_returning<'e>(
        self,
        query: InsertQuery,
    ) -> BoxFuture<'e, Result<InsertReturningPool, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(async move {
            let mut tx = self.begin().await?;
            let row =
                super::insert_returning_cow_tx(&mut tx, std::borrow::Cow::Owned(query)).await?;
            tx.commit().await?;
            Ok(row)
        })
    }

    fn bulk_insert<'e>(
        self,
        query: BulkInsertQuery,
    ) -> BoxFuture<'e, Result<Vec<InsertReturningPool>, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(async move {
            let mut tx = self.begin().await?;
            let rows = super::bulk_insert_rows_tx(&mut tx, std::borrow::Cow::Owned(query)).await?;
            tx.commit().await?;
            Ok(rows)
        })
    }
}

impl<'c> sealed::Sealed<'c> for &'c mut PoolTx<'_> {}
impl<'c> SqlExecutor<'c> for &'c mut PoolTx<'_> {
    fn dialect(&self) -> &'static dyn Dialect {
        PoolTx::dialect(self)
    }

    fetch_rows_or_count!(me => &mut *me);

    fn guard(&self) -> Result<Guard, ExecError> {
        atomic::gate(self).map(Guard)
    }

    fn fetch_rows<'e, 'q: 'e, U, F>(
        self,
        sql: &'q str,
        params: crate::sql::BindParams,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
    where
        'c: 'e,
        U: Send + 'e,
        F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send + 'e,
    {
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            TxConn::Postgres(t) => pg_rows(&mut **t, sql, params, decode),
            #[cfg(feature = "mysql")]
            TxConn::Mysql(t) => my_rows(&mut **t, sql, params, decode),
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(t) => sq_rows(&mut **t, sql, params, decode),
        }
    }

    fn execute_rows<'e, 'q: 'e>(
        self,
        sql: &'q str,
        params: crate::sql::BindParams,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        match &mut self.conn {
            #[cfg(feature = "postgres")]
            TxConn::Postgres(t) => pg_exec(&mut **t, sql, params),
            #[cfg(feature = "mysql")]
            TxConn::Mysql(t) => my_exec(&mut **t, sql, params),
            #[cfg(feature = "sqlite")]
            TxConn::Sqlite(t) => sq_exec(&mut **t, sql, params),
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

    fn run_batches<'e>(
        self,
        stmts: Vec<crate::sql::CompiledStatement>,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(super::execute_batches_tx(self, stmts))
    }

    fn insert_returning<'e>(
        self,
        query: InsertQuery,
    ) -> BoxFuture<'e, Result<InsertReturningPool, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(async move {
            super::insert_returning_cow_tx(self, std::borrow::Cow::Owned(query)).await
        })
    }

    fn bulk_insert<'e>(
        self,
        query: BulkInsertQuery,
    ) -> BoxFuture<'e, Result<Vec<InsertReturningPool>, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(
            async move { super::bulk_insert_rows_tx(self, std::borrow::Cow::Owned(query)).await },
        )
    }
}

// Any Postgres sqlx executor, so `fetch_on(t.conn().await?)` keeps compiling.
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
        params: crate::sql::BindParams,
        decode: F,
    ) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
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
        params: crate::sql::BindParams,
    ) -> BoxFuture<'e, Result<u64, ExecError>>
    where
        'c: 'e,
    {
        pg_exec(self, sql, params)
    }

    fn insert_returning<'e>(
        self,
        query: InsertQuery,
    ) -> BoxFuture<'e, Result<InsertReturningPool, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(async move {
            let row = super::pg_insert_returning(self, std::borrow::Cow::Owned(query)).await?;
            Ok(InsertReturningPool::PgRow(row))
        })
    }

    fn bulk_insert<'e>(
        self,
        query: BulkInsertQuery,
    ) -> BoxFuture<'e, Result<Vec<InsertReturningPool>, ExecError>>
    where
        'c: 'e,
        Self: 'e,
    {
        Box::pin(async move {
            // One executor, one statement: no transaction to span chunks.
            let returns = !query.returning.is_empty();
            let mut stmts = super::compile_bulk_insert_batches(
                crate::sql::postgres::DIALECT,
                std::borrow::Cow::Owned(query),
            )?;
            if stmts.len() != 1 {
                return Err(ExecError::Sql(
                    crate::sql::SqlError::OperatorNotSupportedInDialect {
                        op: "a multi-statement bulk INSERT on a raw sqlx executor",
                        dialect: "postgres",
                    },
                ));
            }
            let stmt = stmts.remove(0);
            crate::test_assertions::query_counter::bump();
            let q = super::bind_pg(sqlx::query(&stmt.sql), stmt.params)?;
            if !returns {
                q.execute(self).await?;
                return Ok(Vec::new());
            }
            Ok(q.fetch_all(self)
                .await?
                .into_iter()
                .map(InsertReturningPool::PgRow)
                .collect())
        })
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

            fetch_rows_or_count!(me => &mut *me);

            fn fetch_rows<'e, 'q: 'e, U, F>(
                self,
                sql: &'q str,
                params: crate::sql::BindParams,
                decode: F,
            ) -> BoxFuture<'e, Result<Vec<U>, ExecError>>
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
                params: crate::sql::BindParams,
            ) -> BoxFuture<'e, Result<u64, ExecError>>
            where
                'c: 'e,
            {
                $exec(&mut ***self, sql, params)
            }

            fn runs_plans(&self) -> bool {
                true
            }

            fn run_delete_plan<'e>(self, plan: DeletePlan) -> BoxFuture<'e, Result<u64, ExecError>>
            where
                'c: 'e,
            {
                Box::pin(async move {
                    let pool = self.pool_id().clone();
                    let mut tx = PoolTx::begin_on(&mut ***self, &pool).await?;
                    let n = super::run_delete_plan_tx(&mut tx, plan).await?;
                    tx.commit().await?;
                    Ok(n)
                })
            }

            fn run_batches<'e>(
                self,
                stmts: Vec<crate::sql::CompiledStatement>,
            ) -> BoxFuture<'e, Result<u64, ExecError>>
            where
                'c: 'e,
                Self: 'e,
            {
                Box::pin(async move {
                    if let [stmt] = stmts.as_slice() {
                        return super::write_on(self, &stmt.sql, stmt.params.clone()).await;
                    }
                    let pool = self.pool_id().clone();
                    let mut tx = PoolTx::begin_on(&mut ***self, &pool).await?;
                    let n = super::execute_batches_tx(&mut tx, stmts).await?;
                    tx.commit().await?;
                    Ok(n)
                })
            }

            fn insert_returning<'e>(
                self,
                query: InsertQuery,
            ) -> BoxFuture<'e, Result<InsertReturningPool, ExecError>>
            where
                'c: 'e,
                Self: 'e,
            {
                Box::pin(async move {
                    let pool = self.pool_id().clone();
                    let mut tx = PoolTx::begin_on(&mut ***self, &pool).await?;
                    let row = super::insert_returning_cow_tx(&mut tx, std::borrow::Cow::Owned(query)).await?;
                    tx.commit().await?;
                    Ok(row)
                })
            }

            fn bulk_insert<'e>(
                self,
                query: BulkInsertQuery,
            ) -> BoxFuture<'e, Result<Vec<InsertReturningPool>, ExecError>>
            where
                'c: 'e,
                Self: 'e,
            {
                Box::pin(async move {
                    let pool = self.pool_id().clone();
                    let mut tx = PoolTx::begin_on(&mut ***self, &pool).await?;
                    let rows = super::bulk_insert_rows_tx(&mut tx, std::borrow::Cow::Owned(query)).await?;
                    tx.commit().await?;
                    Ok(rows)
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
