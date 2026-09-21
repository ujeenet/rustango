//! Django-style `.values()` and `.values_list()`: fetch chosen
//! columns as plain data instead of decoding whole models.
//!
//! Each backend decodes a cell into a `SqlValue` its own way, then
//! the `fetch_values_*` functions shape the rows into dicts, tuples
//! or a flat list.

#[cfg(feature = "postgres")]
use super::bind_query_as;
#[cfg(feature = "mysql")]
use super::bind_query_as_my;
#[cfg(feature = "sqlite")]
use super::bind_query_as_sqlite;
#[cfg(feature = "postgres")]
use sqlx::postgres::{PgArguments, PgRow};
#[cfg(feature = "postgres")]
use sqlx::query::Query;

use super::ExecError;
use crate::core::{AggregateQuery, SelectQuery, SqlValue};
use crate::sql::Pool;
use crate::sql::{
    MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
    UpdaterPool as _,
};

#[cfg(feature = "postgres")]
use super::bind_query;
#[cfg(feature = "mysql")]
use super::bind_query_my;
#[cfg(feature = "sqlite")]
use super::bind_query_sqlite;

/// Decode column `i` of a row into a `SqlValue`. It tries scalars
/// first and JSON after, the same order the aggregate decoder uses,
/// so both agree on mixed types. A NULL or an unknown type gives
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
    } else if let Ok(v) = row.try_get::<rust_decimal::Decimal, _>(i) {
        // `NUMERIC`, which PG widens a `SUM` to. Neither the i64 nor
        // the f64 probe decodes it, so it would come back as Null.
        SqlValue::Decimal(v)
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
    use sqlx::{Column as _, Row as _, TypeInfo as _};
    // `COUNT(*)` and the window ranking functions return BIGINT
    // UNSIGNED, which the i64 probe cannot decode. sqlx's permissive
    // bool decode would then claim it and lose the number, so check
    // the column type first. A real boolean column is not UNSIGNED,
    // so it still reaches the bool probe below.
    if row.column(i).type_info().name().contains("UNSIGNED") {
        if let Ok(v) = row.try_get::<u64, _>(i) {
            // A value above i64::MAX becomes a string rather than
            // saturating, though no real rank gets that large.
            return i64::try_from(v)
                .map_or_else(|_| SqlValue::String(v.to_string()), SqlValue::I64);
        }
        if let Ok(v) = row.try_get::<u32, _>(i) {
            return SqlValue::I64(i64::from(v));
        }
    }
    if let Ok(v) = row.try_get::<i64, _>(i) {
        SqlValue::I64(v)
    } else if let Ok(v) = row.try_get::<i32, _>(i) {
        SqlValue::I32(v)
    } else if let Ok(v) = row.try_get::<f64, _>(i) {
        SqlValue::F64(v)
    } else if let Ok(v) = row.try_get::<bool, _>(i) {
        SqlValue::Bool(v)
    } else if let Ok(v) = row.try_get::<rust_decimal::Decimal, _>(i) {
        // `DECIMAL`, which a `SUM` over an integer column returns.
        // Neither the i64 nor the f64 probe decodes it.
        SqlValue::Decimal(v)
    } else if let Ok(v) = row.try_get::<String, _>(i) {
        SqlValue::String(v)
    } else {
        SqlValue::Null
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_cell_to_sqlvalue(row: &sqlx::sqlite::SqliteRow, i: usize) -> SqlValue {
    use sqlx::{Row as _, TypeInfo as _, ValueRef as _};
    // SQLite is dynamically typed, and an expression column such as
    // a scalar subquery has a storage class but no useful declared
    // type. `try_get::<T>` checks against the declared type, which
    // reads as INTEGER here, so `try_get::<i64>` would turn a text
    // value into `0` while `try_get::<String>` would fail outright.
    //
    // So when the runtime storage class is TEXT, read the bytes with
    // `try_get_unchecked`. Everything else goes to the probe below,
    // which handles it correctly — including correlated aggregates,
    // whose `type_info()` SQLite also misreports.
    if let Ok(raw) = row.try_get_raw(i) {
        let is_null = raw.is_null();
        let is_text = raw.type_info().name() == "TEXT";
        // Decode while `raw` is still in scope: calling `type_info()`
        // inline just before this can make it fail for no reason.
        let as_text = row.try_get_unchecked::<String, _>(i);
        if is_text {
            return if is_null {
                SqlValue::Null
            } else {
                as_text.map_or(SqlValue::Null, SqlValue::String)
            };
        }
    }
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

/// Run a [`SelectQuery`] with a projection and return each row as a
/// map from column name to value. Backs
/// [`crate::query::ValuesQuerySet::fetch`].
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

/// [`fetch_values_dict`] for an [`AggregateQuery`]: each row comes
/// back keyed by output-column alias. Backs
/// [`crate::query::AggregateBuilder::fetch`].
///
/// It goes through [`Pool`], so `.annotate(…)` works on every
/// backend. [`super::fetch_aggregate_on`] does the same on a
/// borrowed Postgres executor.
///
/// # Errors
/// SQL compilation or driver failure.
pub async fn fetch_aggregate_dict(
    pool: &Pool,
    query: &AggregateQuery,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
    let stmt = pool.dialect().compile_aggregate(query)?;
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

/// Run a [`SelectQuery`] with a projection and return each row as a
/// `Vec<SqlValue>` in the projection's column order. Backs
/// [`crate::query::ValuesListQuerySet::fetch`].
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

/// The bound `.values_list_flat::<U>(…)` needs on Postgres. With the
/// feature off it is an empty blanket impl, so such builds compile.
/// See [`super::MaybePgFromRow`].
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

/// Run a one-column [`SelectQuery`] and decode each row's only cell
/// into `U`, as Django's `.values_list('col', flat=True)` does.
///
/// # Errors
/// SQL compilation or driver failure, including a decode error when
/// `U` does not match the column's type.
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

/// Run a two-column [`SelectQuery`] and decode each row into
/// `(K, V)`. Backs [`crate::query::QuerySet::pluck_pairs`].
///
/// # Errors
/// SQL compilation or driver failure, including a decode error when
/// `K` or `V` does not match its column's type.
pub async fn fetch_values_pairs<K, V>(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<(K, V)>, ExecError>
where
    K: Send + Unpin,
    V: Send + Unpin,
    (K, V): MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::Postgres, (K, V), PgArguments> =
                sqlx::query_as::<_, (K, V)>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, (K, V), sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, (K, V)>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_my(q, v);
            }
            Ok(q.fetch_all(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryAs<
                '_,
                sqlx::Sqlite,
                (K, V),
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, (K, V)>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_sqlite(q, v);
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
    /// Run the projection and return one map per row.
    ///
    /// # Errors
    /// - [`ExecError::Query`] when the SQL fails to compile, for
    ///   example on a misspelled column.
    /// - [`ExecError::Driver`] for driver, network or decode failures.
    pub async fn fetch(
        self,
        pool: &Pool,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
        let q = self.compile()?;
        fetch_values_dict(pool, &q).await
    }
}

impl<T: crate::core::Model> crate::query::ValuesQuerySet<T> {
    /// [`Self::fetch`] on a borrowed executor instead of the pool.
    /// Use it for a tenant-scoped query: a schema-mode tenant is
    /// selected by `SET search_path` on one connection, so going
    /// through the pool would read `public` instead.
    ///
    /// # Errors
    /// As [`Self::fetch`].
    #[cfg(feature = "postgres")]
    pub async fn fetch_on<'c, E>(
        self,
        executor: E,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let query = self.compile()?;
        let stmt = {
            use crate::sql::Dialect as _;
            crate::sql::Postgres.compile_select(&query)?
        };
        let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
        for v in stmt.params {
            q = bind_query(q, v);
        }
        let rows = q.fetch_all(executor).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            use sqlx::{Column as _, Row as _};
            let mut map = std::collections::HashMap::new();
            for (i, col) in row.columns().iter().enumerate() {
                map.insert(col.name().to_owned(), pg_cell_to_sqlvalue(row, i));
            }
            out.push(map);
        }
        Ok(out)
    }
}

impl<T: crate::core::Model> crate::query::AggregateBuilder<T> {
    /// Run the aggregate query and return one map per row, keyed by
    /// output-column name.
    ///
    /// This backs every `.aggregate()` and `.annotate(…)` chain,
    /// including [`crate::query::QuerySet::annotate_count`] and its
    /// siblings, where each row carries the parent's own columns plus
    /// the derived one.
    ///
    /// # Errors
    /// - [`ExecError::Query`] when the SQL fails to compile, for
    ///   example on an unknown relation or a bad GROUP BY.
    /// - [`ExecError::Driver`] for driver, network or decode failures.
    pub async fn fetch(
        self,
        pool: &Pool,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
        let q = self.compile()?;
        fetch_aggregate_dict(pool, &q).await
    }

    /// [`Self::fetch`] on a borrowed executor instead of the pool,
    /// for a tenant-scoped query. See
    /// [`crate::query::QuerySet::fetch_on`].
    ///
    /// ```ignore
    /// SetLog::objects()
    ///     .values(&["member_id"])
    ///     .annotate("total", sum("volume").into())
    ///     .fetch_on(t.conn())   // runs in the tenant's schema
    ///     .await?;
    /// ```
    ///
    /// # Errors
    /// As [`Self::fetch`].
    #[cfg(feature = "postgres")]
    pub async fn fetch_on<'c, E>(
        self,
        executor: E,
    ) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let q = self.compile()?;
        super::fetch_aggregate_on(&q, executor).await
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

impl<T: crate::core::Model> crate::query::QuerySet<T> {
    /// Read one column from the matching rows into a `Vec<U>`.
    ///
    /// ```ignore
    /// let titles: Vec<String> = Post::objects()
    ///     .filter("published", true)
    ///     .pluck::<String>("title", &pool).await?;
    /// ```
    ///
    /// `U` must decode from the column's type on every backend you
    /// target; `i64`, `String`, `bool` and `f64` are the usual
    /// choices. Unlike `Model::pluck`, this respects the queryset's
    /// filters, ordering and limits.
    ///
    /// # Errors
    /// As [`crate::query::ValuesFlatQuerySet::fetch`].
    pub async fn pluck<U>(self, col: &'static str, pool: &Pool) -> Result<Vec<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        self.values_list_flat(col).fetch::<U>(pool).await
    }

    /// [`Self::pluck`] of the primary key, whose name comes from the
    /// model's schema so the call site need not repeat it.
    ///
    /// ```ignore
    /// let ids: Vec<i64> = Post::objects()
    ///     .filter("published", true)
    ///     .pks::<i64>(&pool).await?;
    /// ```
    ///
    /// # Errors
    /// [`ExecError::Query(QueryError::UnknownField)`] when the model
    /// has no primary key, otherwise as [`Self::pluck`].
    pub async fn pks<K>(self, pool: &Pool) -> Result<Vec<K>, ExecError>
    where
        K: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        let pk_col = T::SCHEMA.primary_key().map(|f| f.column).ok_or_else(|| {
            ExecError::Query(crate::core::QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: "<pk>".to_string(),
            })
        })?;
        self.pluck::<K>(pk_col, pool).await
    }

    /// Read two columns from the matching rows into `Vec<(K, V)>`.
    /// Collect it into a map at the call site if you want lookups.
    /// The key column comes first, matching the tuple.
    ///
    /// ```ignore
    /// let pairs: Vec<(i64, String)> = Post::objects()
    ///     .filter("published", true)
    ///     .pluck_pairs::<i64, String>("id", "title", &pool).await?;
    /// let map: std::collections::BTreeMap<_, _> = pairs.into_iter().collect();
    /// ```
    ///
    /// # Errors
    /// As [`crate::sql::FetcherPool::fetch`], plus a decode error
    /// when `K` or `V` does not match its column's type.
    pub async fn pluck_pairs<K, V>(
        self,
        key_col: &'static str,
        value_col: &'static str,
        pool: &Pool,
    ) -> Result<Vec<(K, V)>, ExecError>
    where
        K: Send + Unpin,
        V: Send + Unpin,
        (K, V): MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
    {
        let q = self.values_list(&[key_col, value_col]).compile()?;
        fetch_values_pairs::<K, V>(pool, &q).await
    }

    /// Read one column from the first matching row, or `None` when
    /// nothing matches. The query carries `LIMIT 1`, so the database
    /// does not build rows you will not read.
    ///
    /// ```ignore
    /// let email: Option<String> = User::objects()
    ///     .filter("id", 1_i64)
    ///     .value::<String>("email", &pool).await?;
    /// ```
    ///
    /// Unlike `Model::value`, this respects the queryset's filters
    /// and ordering when picking the row.
    ///
    /// # Errors
    /// As [`crate::query::ValuesFlatQuerySet::first`].
    pub async fn value<U>(self, col: &'static str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        self.values_list_flat(col).first::<U>(pool).await
    }

    /// Render this queryset to SQL in the pool's dialect, without
    /// running it. Handy for debugging, logging and snapshot tests.
    ///
    /// The string holds placeholders, not the bound values. Use
    /// [`Self::to_compiled`] when you need those too.
    ///
    /// ```ignore
    /// let sql = Post::objects()
    ///     .filter("published", true)
    ///     .order_by(&[("created_at", true)])
    ///     .limit(10)
    ///     .to_sql(&pool)?;
    /// // -> "SELECT … FROM \"post\" WHERE \"published\" = $1 …"
    /// ```
    ///
    /// # Errors
    /// [`ExecError::Query`] when the queryset fails to compile, and
    /// [`ExecError::Sql`] from the dialect's writer.
    pub fn to_sql(self, pool: &Pool) -> Result<String, ExecError> {
        let q = self.compile()?;
        let stmt = pool.dialect().compile_select(&q)?;
        Ok(stmt.sql)
    }

    /// `SELECT SUM(col)` over the matching rows, or `Ok(None)` when
    /// none match. Unlike `Model::sum`, this respects the queryset's
    /// filters.
    ///
    /// # Errors
    /// As [`crate::sql::fetch_aggregate_pool`], plus
    /// [`ExecError::Query(QueryError::UnknownField)`] when the model
    /// has no such column.
    pub async fn sum<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Sum, pool)
            .await
    }

    /// Fetch one page of rows and the matching total, as
    /// `(rows, total)`. Unlike `Model::paginate`, the total counts
    /// only rows the queryset's filters match.
    ///
    /// It runs two queries, a `COUNT(*)` and the page itself, both
    /// with the same WHERE clause.
    ///
    /// `page` starts at 1, so `paginate(1, 10)` gives the first ten
    /// rows and `paginate(2, 10)` the next ten.
    ///
    /// # Errors
    /// As [`crate::sql::CounterPool::count`] and
    /// [`crate::sql::FetcherPool::fetch`].
    pub async fn paginate(
        self,
        page: i64,
        per_page: i64,
        pool: &Pool,
    ) -> Result<(Vec<T>, i64), ExecError>
    where
        T: crate::core::Model
            + crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + crate::sql::LoadRelated
            + crate::sql::MaybeMyLoadRelated
            + crate::sql::MaybeSqliteLoadRelated
            + Send
            + Unpin,
    {
        use crate::sql::{CounterPool as _, FetcherPool as _};
        let total = <crate::query::QuerySet<T> as ::core::clone::Clone>::clone(&self)
            .count(pool)
            .await?;
        let offset = if page > 1 { (page - 1) * per_page } else { 0 };
        let rows = self.limit(per_page).offset(offset).fetch(pool).await?;
        Ok((rows, total))
    }

    /// [`Self::sum`] as an average.
    ///
    /// # Errors
    /// As [`Self::sum`].
    pub async fn avg<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Avg, pool)
            .await
    }

    /// [`Self::sum`] as a minimum.
    ///
    /// # Errors
    /// As [`Self::sum`].
    pub async fn min<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Min, pool)
            .await
    }

    /// [`Self::sum`] as a maximum.
    ///
    /// # Errors
    /// As [`Self::sum`].
    pub async fn max<U>(self, col: &str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        self.queryset_aggregate_one::<U>(col, crate::core::AggregateExpr::Max, pool)
            .await
    }

    /// Shared body of `sum`, `avg`, `min` and `max`. It checks the
    /// column against the model, moves the queryset's filters into
    /// the aggregate's WHERE clause, and returns the one value.
    async fn queryset_aggregate_one<U>(
        self,
        col: &str,
        build: fn(&'static str) -> crate::core::AggregateExpr,
        pool: &Pool,
    ) -> Result<Option<U>, ExecError>
    where
        (Option<U>,): crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + Send
            + Unpin,
    {
        let col_static = crate::sql::model_shortcuts::resolve_col::<T>(col)?;
        // Compile the queryset to get its WHERE clause, then build
        // the AggregateQuery by hand so its projection is exactly
        // one aggregate column and the tuple decode lines up.
        let select_q = self.compile()?;
        let aggregate_q = crate::core::AggregateQuery {
            model: <T as crate::core::Model>::SCHEMA,
            // Keep the queryset's joins so an aggregate over a
            // joined column still resolves.
            joins: select_q.joins,
            where_clause: select_q.where_clause,
            group_by: Vec::new(),
            aggregates: vec![("v".into(), build(col_static))],
            aliases: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        };
        let rows: Vec<(Option<U>,)> = crate::sql::fetch_aggregate_pool(pool, &aggregate_q).await?;
        Ok(rows.into_iter().next().and_then(|t| t.0))
    }

    /// [`Self::to_sql`] plus the bound parameters, as a
    /// [`crate::sql::CompiledStatement`].
    ///
    /// # Errors
    /// As [`Self::to_sql`].
    pub fn to_compiled(self, pool: &Pool) -> Result<crate::sql::CompiledStatement, ExecError> {
        let q = self.compile()?;
        let stmt = pool.dialect().compile_select(&q)?;
        Ok(stmt)
    }
}

impl<T> crate::query::QuerySet<T>
where
    T: crate::core::Model
        + Send
        + Unpin
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated,
{
    /// `UPDATE … SET col = col + by` over the matching rows.
    /// Returns the number of rows affected. Unlike
    /// `Model::increment_each`, this respects the queryset's
    /// filters, so you can bump a counter on some rows only.
    ///
    /// A negative `by` subtracts; [`Self::decrement`] reads better
    /// at the call site.
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter("published", true)
    ///     .increment("views", 1, &pool)
    ///     .await?;
    /// ```
    ///
    /// # Errors
    /// As [`UpdaterPool::execute_pool`](crate::sql::UpdaterPool::execute_pool),
    /// plus [`ExecError::Query`] with `QueryError::UnknownField` when
    /// `col` is not a declared field on `T`.
    pub async fn increment(self, col: &str, by: i64, pool: &Pool) -> Result<u64, ExecError> {
        let col_static = crate::sql::model_shortcuts::resolve_col::<T>(col)?;
        self.update()
            .set_expr(
                col,
                crate::sql::model_shortcuts::add_signed_expr(col_static, by),
            )
            .execute_pool(pool)
            .await
    }

    /// [`Self::increment`] the other way, subtracting `by`.
    ///
    /// ```ignore
    /// User::objects()
    ///     .filter("vip", true)
    ///     .decrement("credits", 10, &pool)
    ///     .await?;
    /// ```
    ///
    /// # Errors
    /// As [`Self::increment`].
    pub async fn decrement(self, col: &str, by: i64, pool: &Pool) -> Result<u64, ExecError> {
        self.increment(col, -by, pool).await
    }
}

impl<T: crate::core::Model> crate::query::ValuesFlatQuerySet<T> {
    /// Run the one-column projection and decode each cell into `U`.
    ///
    /// # Errors
    /// As [`crate::query::ValuesQuerySet::fetch`], plus a decode
    /// error when `U` does not match the column's type.
    pub async fn fetch<U>(self, pool: &Pool) -> Result<Vec<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        let q = self.compile()?;
        fetch_values_flat::<U>(pool, &q).await
    }

    /// Return the first row's cell, or `None` when nothing matches.
    /// It adds `LIMIT 1`, so unlike [`Self::fetch`] the database
    /// does not build rows you will not read.
    ///
    /// ```ignore
    /// let name: Option<String> = User::query()
    ///     .filter("id", 1_i64)
    ///     .values_list_flat("name")
    ///     .first::<String>(&pool).await?;
    /// ```
    ///
    /// # Errors
    /// As [`Self::fetch`].
    pub async fn first<U>(self, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        // Rebuild with `LIMIT 1`. `compile()` consumes `self.qs`, so
        // the limit has to go on through the builder.
        let col = self.col;
        let qs = self.qs.limit(1);
        let q = crate::query::ValuesFlatQuerySet { qs, col }.compile()?;
        let rows = fetch_values_flat::<U>(pool, &q).await?;
        Ok(rows.into_iter().next())
    }
}
