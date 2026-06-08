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

impl<T: crate::core::Model> crate::query::QuerySet<T> {
    /// Eloquent `Builder::pluck($col)` — single-column projection
    /// on this queryset, decoded into `Vec<U>`. Sugar over
    /// `self.values_list_flat(col).fetch::<U>(pool).await`.
    ///
    /// ```ignore
    /// // Eloquent: Post::where('published', true)->pluck('title');
    /// // rustango:
    /// let titles: Vec<String> = Post::objects()
    ///     .filter("published", true)
    ///     .pluck::<String>("title", &pool).await?;
    /// ```
    ///
    /// `U` must be decodable from the column's SQL type on every
    /// dialect the binary targets — common picks: `i64` / `i32` /
    /// `String` / `bool` / `f64`.
    ///
    /// Differs from `Model::pluck(col, &pool)` (already shipped) in
    /// that the static-method form scans every row of the table;
    /// this method's queryset can carry filters / ordering / limits
    /// so you can pluck a column from a narrowed result set.
    ///
    /// # Errors
    /// As [`crate::query::ValuesFlatQuerySet::fetch`].
    pub async fn pluck<U>(self, col: &'static str, pool: &Pool) -> Result<Vec<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        self.values_list_flat(col).fetch::<U>(pool).await
    }

    /// Eloquent `Builder::value($col)` — fetch a single column from
    /// the first row of this (possibly-filtered) queryset, or
    /// `None` when the queryset is empty.
    ///
    /// Sugar over `self.values_list_flat(col).first::<U>(pool)`. The
    /// DB sees `LIMIT 1` so a large result set doesn't pay for rows
    /// the caller won't read.
    ///
    /// ```ignore
    /// // Eloquent: $email = User::where('id', 1)->value('email');
    /// // rustango:
    /// let email: Option<String> = User::objects()
    ///     .filter("id", 1_i64)
    ///     .value::<String>("email", &pool).await?;
    /// ```
    ///
    /// Differs from `Model::value(col, &pool)` (already shipped)
    /// because the queryset's accumulated filters / ordering /
    /// limits narrow which row's column is returned.
    ///
    /// # Errors
    /// As [`crate::query::ValuesFlatQuerySet::first`].
    pub async fn value<U>(self, col: &'static str, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        self.values_list_flat(col).first::<U>(pool).await
    }

    /// Eloquent `Builder::toSql()` — render this queryset to its SQL
    /// string in the pool's dialect, without executing.
    ///
    /// Placeholders use the dialect-specific syntax (`$1` / `$2` on
    /// PG, `?` on MySQL / SQLite). Bind values are NOT included in
    /// the returned string — use [`Self::to_compiled`] if you need
    /// the parameter list too.
    ///
    /// Useful for debugging, logging, snapshot tests, and copying
    /// the SQL into a database client to run by hand.
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
    /// Returns [`ExecError::Query`] if the queryset compile step
    /// fails (e.g. unknown field, type mismatch) and
    /// [`ExecError::Sql`] from the dialect's writer.
    pub fn to_sql(self, pool: &Pool) -> Result<String, ExecError> {
        let q = self.compile()?;
        let stmt = pool.dialect().compile_select(&q)?;
        Ok(stmt.sql)
    }

    /// Eloquent `Builder::sum($col)` on a filtered queryset —
    /// `SELECT SUM(col) FROM <table> WHERE …`. Returns `Ok(None)`
    /// when the filtered result set is empty.
    ///
    /// Differs from `Model::sum(col, &pool)` (already shipped)
    /// which sums over every row of the table; this method respects
    /// the queryset's accumulated filters.
    ///
    /// # Errors
    /// As [`crate::sql::fetch_aggregate_pool`]; plus
    /// [`ExecError::Query(QueryError::UnknownField)`] when `col`
    /// isn't declared on the model.
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

    /// Eloquent `Builder::paginate($per_page, $page)` on a
    /// **filtered** queryset — fetch one page of rows AND the
    /// filtered total in a single call. Returns `(rows, total)`.
    ///
    /// Counterpart of the table-wide `Model::paginate`: this
    /// version respects the queryset's accumulated filters so the
    /// `total` reflects "matching rows" rather than "every row of
    /// the table".
    ///
    /// Two queries under the hood: `SELECT COUNT(*) FROM … WHERE …`
    /// for the total, then `SELECT … FROM … WHERE … LIMIT N OFFSET
    /// M` for the page. The queryset is cloned for the count step
    /// so the page-fetch's filter chain stays intact — both halves
    /// see the same WHERE.
    ///
    /// 1-indexed `page` so `paginate(1, 10)` returns rows 0..10,
    /// `paginate(2, 10)` returns rows 10..20, etc. — matches
    /// Eloquent and `Model::for_page`.
    ///
    /// # Errors
    /// As [`crate::sql::CounterPool::count_pool`] and
    /// [`crate::sql::FetcherPool::fetch_pool`].
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
            .count_pool(pool)
            .await?;
        let offset = if page > 1 { (page - 1) * per_page } else { 0 };
        let rows = self.limit(per_page).offset(offset).fetch_pool(pool).await?;
        Ok((rows, total))
    }

    /// Eloquent `Builder::avg($col)` on a filtered queryset.
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

    /// Eloquent `Builder::min($col)` on a filtered queryset.
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

    /// Eloquent `Builder::max($col)` on a filtered queryset.
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

    /// Internal: shared aggregate-on-queryset helper for `sum` /
    /// `avg` / `min` / `max`. Validates the column against the
    /// model schema, lifts the queryset's filters into the
    /// aggregate's WHERE clause, then runs through
    /// `fetch_aggregate_pool` and extracts the single column value.
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
        // Lower this queryset to a SELECT to grab its WHERE clause,
        // then hand-build the AggregateQuery so the aggregate's
        // projection is exactly `<build>(col)` (no extra columns) —
        // matches the shape `aggregate_one_pool` uses table-wide so
        // the `Vec<(Option<U>,)>` decode lines up.
        let select_q = self.compile()?;
        let aggregate_q = crate::core::AggregateQuery {
            model: <T as crate::core::Model>::SCHEMA,
            where_clause: select_q.where_clause,
            group_by: Vec::new(),
            aggregates: vec![("v", build(col_static))],
            aliases: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        };
        let rows: Vec<(Option<U>,)> = crate::sql::fetch_aggregate_pool(pool, &aggregate_q).await?;
        Ok(rows.into_iter().next().and_then(|t| t.0))
    }

    /// Like [`Self::to_sql`] but returns the full
    /// [`crate::sql::CompiledStatement`] (SQL + bound parameters).
    /// Use this when you need the binds — e.g. logging both halves
    /// of a parameterized query or building a snapshot test that
    /// asserts on exact placeholder shape.
    ///
    /// # Errors
    /// As [`Self::to_sql`].
    pub fn to_compiled(self, pool: &Pool) -> Result<crate::sql::CompiledStatement, ExecError> {
        let q = self.compile()?;
        let stmt = pool.dialect().compile_select(&q)?;
        Ok(stmt)
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

    /// Execute the projection and return the first row's cell, or
    /// `None` when the queryset is empty. Eloquent `Builder::value()`
    /// parity — the one-row-one-column shortcut.
    ///
    /// ```ignore
    /// // Eloquent: $name = User::where('id', 1)->value('name');
    /// // rustango:
    /// let name: Option<String> = User::query()
    ///     .filter("id", 1_i64)
    ///     .values_list_flat("name")
    ///     .first::<String>(&pool).await?;
    /// ```
    ///
    /// Equivalent to `.fetch::<U>(pool).await?.into_iter().next()`.
    /// The full-fetch path materializes every matching row; this
    /// helper appends `LIMIT 1` to the underlying queryset so a
    /// large result set doesn't pay for rows the caller won't read.
    ///
    /// # Errors
    /// As [`Self::fetch`].
    pub async fn first<U>(self, pool: &Pool) -> Result<Option<U>, ExecError>
    where
        U: MaybePgScalar + MaybeMyScalar + MaybeSqliteScalar + Send + Unpin,
    {
        // Re-build with a LIMIT 1 on the underlying queryset so the
        // DB doesn't materialize rows past the first one. `compile()`
        // consumes `self.qs`, so we need to take the limit detour
        // through the builder.
        let col = self.col;
        let qs = self.qs.limit(1);
        let q = crate::query::ValuesFlatQuerySet { qs, col }.compile()?;
        let rows = fetch_values_flat::<U>(pool, &q).await?;
        Ok(rows.into_iter().next())
    }
}
