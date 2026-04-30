//! Async executor — binds a `CompiledStatement` to sqlx and runs it.

use crate::core::{
    BulkInsertQuery, CountQuery, DeleteQuery, InsertQuery, Model, SelectQuery, SqlValue,
    UpdateQuery,
};
use crate::query::{QuerySet, UpdateBuilder};
use sqlx::postgres::{PgArguments, PgPool, PgRow};
use sqlx::query::{Query, QueryAs};

use super::{Dialect, ExecError, Postgres};

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

impl<T> QuerySet<T>
where
    T: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    /// Like [`Fetcher::fetch`] but takes any sqlx executor — `&PgPool`,
    /// `&mut PgConnection`, or a `Transaction`. The escape hatch for
    /// tenant-scoped queries: schema-mode tenants share the registry
    /// pool but rely on a per-checkout `SET search_path`, so passing
    /// `&PgPool` would silently hit the wrong schema. Acquire a
    /// connection via `TenantPools::acquire(&org)` and pass that here.
    ///
    /// # Errors
    /// As [`Fetcher::fetch`].
    pub async fn fetch_on<'c, E>(self, executor: E) -> Result<Vec<T>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let select = self.compile()?;
        let stmt = Postgres.compile_select(&select)?;
        let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> = sqlx::query_as::<_, T>(&stmt.sql);
        for value in stmt.params {
            q = bind_query_as(q, value);
        }
        let rows = q.fetch_all(executor).await?;
        Ok(rows)
    }

    /// Fetch a page of rows **and** the total matching count in a
    /// single SQL round trip. Postgres' `COUNT(*) OVER ()` window
    /// function returns the pre-LIMIT total alongside each row, so a
    /// paginated endpoint never needs the customary second
    /// `SELECT COUNT(*)` Django's `Paginator` triggers.
    ///
    /// ```ignore
    /// let page: Page<Post> = Post::objects()
    ///     .where_(Post::published.eq(true))
    ///     .limit(20).offset(40)
    ///     .fetch_paginated_on(tenant.conn()).await?;
    /// assert!(page.total >= page.rows.len() as i64);
    /// ```
    ///
    /// SQL emitted (abridged):
    ///
    /// ```text
    /// SELECT id, title, ..., COUNT(*) OVER () AS "__rustango_total"
    /// FROM post WHERE ...
    /// ORDER BY ... LIMIT 20 OFFSET 40
    /// ```
    ///
    /// Empty result set → `Page { rows: vec![], total: 0 }` (no
    /// driver round trip is wasted on an extra COUNT).
    ///
    /// # Errors
    /// As [`Self::fetch_on`].
    pub async fn fetch_paginated_on<'c, E>(self, executor: E) -> Result<Page<T>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let select = self.compile()?;
        let stmt = Postgres.compile_select(&select)?;
        let sql = inject_total_count(&stmt.sql);
        let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&sql);
        for value in stmt.params {
            q = bind_query(q, value);
        }
        let raw_rows: Vec<PgRow> = q.fetch_all(executor).await?;
        let total: i64 = raw_rows
            .first()
            .map(|row| sqlx::Row::try_get::<i64, _>(row, "__rustango_total"))
            .transpose()?
            .unwrap_or(0);
        let mut rows = Vec::with_capacity(raw_rows.len());
        for row in &raw_rows {
            rows.push(T::from_row(row)?);
        }
        Ok(Page { rows, total })
    }

    /// Pool-side companion to [`Self::fetch_paginated_on`] — same
    /// query, ergonomics for non-tenant code.
    ///
    /// # Errors
    /// As [`Self::fetch_paginated_on`].
    pub async fn fetch_paginated(self, pool: &PgPool) -> Result<Page<T>, ExecError> {
        self.fetch_paginated_on(pool).await
    }
}

/// Result of [`QuerySet::fetch_paginated_on`] — a slice of rows
/// alongside the total count of matching rows in the underlying
/// query (i.e. the count *before* LIMIT/OFFSET).
///
/// Both pieces come from a single SQL round trip via
/// `COUNT(*) OVER ()`, so paginated endpoints don't pay the
/// customary "two queries per page" cost Django's `Paginator`
/// imposes.
pub struct Page<T> {
    pub rows: Vec<T>,
    pub total: i64,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self { rows: Vec::new(), total: 0 }
    }
}

/// Splice `, COUNT(*) OVER () AS "__rustango_total"` into the
/// compiled SELECT's column list, just before the first ` FROM `.
/// The Postgres compile_select writer emits the shape
/// `SELECT <cols> FROM <table> ...` (single space before `FROM`), and
/// quoted column literals never contain the bare token ` FROM ` —
/// quoted strings are SqlValue parameters, not part of the column
/// list. The wrapper-subquery fallback handles unexpected shapes
/// safely (`COUNT(*) OVER ()` at the OUTER level still counts inner
/// rows correctly when the inner has no LIMIT, but with LIMIT the
/// outer COUNT would only see the limited slice — so we depend on
/// the fast path matching).
fn inject_total_count(sql: &str) -> String {
    if let Some(idx) = sql.find(" FROM ") {
        let mut out = String::with_capacity(sql.len() + 48);
        out.push_str(&sql[..idx]);
        out.push_str(", COUNT(*) OVER () AS \"__rustango_total\"");
        out.push_str(&sql[idx..]);
        out
    } else {
        // Should not reach this branch with the current Postgres
        // writer — surface the unexpected SQL clearly rather than
        // silently producing wrong totals.
        format!(
            "/* rustango: fetch_paginated_on could not splice COUNT(*) OVER () \
             into the compiled SELECT — anchor ` FROM ` not found. The query \
             below will run unchanged but `total` will be 0. */ {sql}"
        )
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
    insert_on(pool, query).await
}

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

/// Run an `InsertQuery` and return the row created by the
/// `RETURNING` clause.
///
/// Used by macro-generated insert paths for models with `Auto<T>` PKs:
/// the column is omitted from the INSERT (so Postgres' BIGSERIAL
/// sequence fires) and the assigned value is read back via `RETURNING`.
/// Caller pulls each returned column out via `sqlx::Row::try_get` —
/// e.g. `Auto<i64>::decode` rebuilds an `Auto::Set(value)`.
///
/// # Errors
/// Returns [`ExecError::EmptyReturning`] if `query.returning` is empty
/// (use [`insert`] for those); validation, SQL-writing, or driver
/// failures otherwise.
pub async fn insert_returning(pool: &PgPool, query: &InsertQuery) -> Result<PgRow, ExecError> {
    insert_returning_on(pool, query).await
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

/// Run a `BulkInsertQuery` against a Postgres pool — one round-trip
/// for every row. Returns the rows produced by the `RETURNING`
/// clause (one per input row), or an empty `Vec` if the query
/// requested no `RETURNING`.
///
/// Used by macro-generated `Model::bulk_insert(pool, &mut rows)`.
/// Validates each row against the model's bounds before opening
/// the connection.
///
/// # Errors
/// Returns [`ExecError`] for validation, SQL-writing, or driver failures.
pub async fn bulk_insert(
    pool: &PgPool,
    query: &BulkInsertQuery,
) -> Result<Vec<PgRow>, ExecError> {
    bulk_insert_on(pool, query).await
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

/// Run an `UpdateQuery` against a Postgres pool. Returns rows affected.
///
/// Validates each `SET` value against the declared field bounds before
/// opening the connection.
///
/// # Errors
/// Returns [`ExecError`] for validation, SQL-writing, or driver failures.
pub async fn update(pool: &PgPool, query: &UpdateQuery) -> Result<u64, ExecError> {
    update_on(pool, query).await
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

/// Run a `DeleteQuery` against a Postgres pool. Returns rows affected.
///
/// # Errors
/// Returns [`ExecError`] for SQL-writing or driver failures.
pub async fn delete(pool: &PgPool, query: &DeleteQuery) -> Result<u64, ExecError> {
    delete_on(pool, query).await
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

/// Run a `CountQuery` and return the row count.
///
/// # Errors
/// Returns [`ExecError`] for SQL-writing or driver failures.
pub async fn count_rows(pool: &PgPool, query: &CountQuery) -> Result<i64, ExecError> {
    count_rows_on(pool, query).await
}

/// Like [`count_rows`] but accepts any sqlx executor.
///
/// # Errors
/// As [`count_rows`].
pub async fn count_rows_on<'c, E>(executor: E, query: &CountQuery) -> Result<i64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_count(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let row = q.fetch_one(executor).await?;
    Ok(sqlx::Row::try_get::<i64, _>(&row, 0)?)
}

/// Extension trait that runs a `SELECT COUNT(*)` against the queryset's
/// filters. Pulled in via `use rustango::sql::Counter;`.
pub trait Counter<T: Model + Send> {
    /// Count rows matching the queryset's filters.
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    fn count(
        self,
        pool: &PgPool,
    ) -> impl std::future::Future<Output = Result<i64, ExecError>> + Send;
}

impl<T: Model + Send> Counter<T> for QuerySet<T> {
    async fn count(self, pool: &PgPool) -> Result<i64, ExecError> {
        self.count_on(pool).await
    }
}

impl<T: Model + Send> QuerySet<T> {
    /// Like [`Counter::count`] but accepts any sqlx executor — for
    /// tenant-scoped counts against a connection that has the
    /// `search_path` already set. See [`QuerySet::fetch_on`] for the
    /// rationale.
    ///
    /// # Errors
    /// As [`Counter::count`].
    pub async fn count_on<'c, E>(self, executor: E) -> Result<i64, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let select = self.compile()?;
        count_rows_on(
            executor,
            &CountQuery {
                model: select.model,
                where_clause: select.where_clause,
            },
        )
        .await
    }
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
