//! Async executor — binds a `CompiledStatement` to sqlx and runs it.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, CountQuery, DeleteQuery, InsertQuery, Model,
    SelectQuery, SqlValue, UpdateQuery,
};
use crate::query::{QuerySet, UpdateBuilder};
use sqlx::postgres::{PgArguments, PgPool, PgRow};
use sqlx::query::{Query, QueryAs};

use super::{Dialect, ExecError, Postgres};

/// Hidden trait every `#[derive(Model)]` type implements via the
/// macro — slice 9.0e's bridge between `fetch_with_prefetch` and
/// the per-Model FK-PK accessor. For each `ForeignKey<T>` field on
/// a Child model, the macro generates an arm that returns the FK's
/// stored PK (regardless of `Loaded` / `Unloaded` state) so the
/// prefetch grouper can stitch children to the right parent.
///
/// Models with no `ForeignKey<T>` fields get a no-op impl
/// (returns `None` for any field name).
#[doc(hidden)]
pub trait FkPkAccess {
    /// Read the i64 PK stored in a `ForeignKey<T>` field by name.
    /// `None` for unknown field names or non-FK fields.
    fn __rustango_fk_pk(&self, field_name: &str) -> Option<i64>;
}

/// Hidden trait every `#[derive(Model)]` type implements via the
/// macro — slice 9.0d's bridge between `QuerySet::fetch_on` and the
/// per-Model `__rustango_load_related` dispatcher. Loaders for
/// individual FK fields live on the Model's inherent impl; this
/// trait makes them callable polymorphically from generic
/// fetch_on code.
///
/// Models with no `ForeignKey<T>` fields get a no-op impl
/// (returns `Ok(false)` for any field name), so the trait bound on
/// `fetch_on` is universally satisfied — users don't have to think
/// about it.
#[doc(hidden)]
pub trait LoadRelated {
    /// Stitch a `select_related`-loaded parent onto this instance's
    /// FK field. `field_name` is the FK field's Rust name (e.g.
    /// `"author"`); `alias` is the SELECT writer's alias prefix
    /// for that JOIN's projected columns (typically the same as
    /// `field_name`). Returns `Ok(false)` for unknown field names —
    /// callers may pass select directives that don't apply to this
    /// model and get a graceful skip.
    ///
    /// # Errors
    /// `sqlx::Error` from `try_get` decoding the joined columns.
    fn __rustango_load_related(
        &mut self,
        row: &PgRow,
        field_name: &str,
        alias: &str,
    ) -> Result<bool, sqlx::Error>;
}

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
        T: LoadRelated,
    {
        let select = self.compile()?;
        let select_related_aliases: Vec<&'static str> =
            select.joins.iter().map(|j| j.alias).collect();
        let stmt = Postgres.compile_select(&select)?;

        if select_related_aliases.is_empty() {
            // No JOINs — fast path identical to the v0.8.1 shape.
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for value in stmt.params {
                q = bind_query_as(q, value);
            }
            let rows = q.fetch_all(executor).await?;
            return Ok(rows);
        }

        // Slice 9.0d: select_related path. Fetch raw rows so we can
        // both decode `T` via `from_row` AND call
        // `T::__rustango_load_related(&mut t, &row, alias, alias)`
        // for each JOINed target — single SQL round trip, no N+1.
        let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
        for value in stmt.params {
            q = bind_query(q, value);
        }
        let raw_rows = q.fetch_all(executor).await?;
        let mut out = Vec::with_capacity(raw_rows.len());
        for row in &raw_rows {
            let mut t = T::from_row(row)?;
            for alias in &select_related_aliases {
                let _ = t.__rustango_load_related(row, alias, alias)?;
            }
            out.push(t);
        }
        Ok(out)
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

/// Slice 9.0b — annotate each parent row with the COUNT of its
/// children, returning `Vec<(Parent, i64)>` from a **single** SQL:
///
/// ```text
///   SELECT parent.<every-column>, COUNT(child.<pk>) AS __annotated_count
///   FROM parent
///   LEFT JOIN child ON child.<fk_column> = parent.<pk>
///   GROUP BY parent.<every-column>
///   [WHERE / ORDER BY clauses from `parent_qs` apply]
/// ```
///
/// Closes the demo's per-parent `count_on` loop (which was N+1) with
/// the canonical Django `Author.objects.annotate(post_count=Count('post'))`
/// shape. Restricted to a single Count aggregate over a single
/// reverse-FK relation in this MVP — full Django aggregation
/// (`.annotate(other_field=Sum(...), Avg(...), ...)`) is queued for
/// a follow-on slice.
///
/// `child_table` is the SQL table of the child model; `child_fk_column`
/// is the column on that table that stores the parent's PK.
///
/// # Errors
/// SQL-writing or driver failures from the single SELECT.
pub async fn annotate_count_children<P>(
    parent_qs: crate::query::QuerySet<P>,
    child_table: &'static str,
    child_fk_column: &'static str,
    pool: &PgPool,
) -> Result<Vec<(P, i64)>, ExecError>
where
    P: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    annotate_count_children_on(parent_qs, child_table, child_fk_column, pool).await
}

/// Like [`annotate_count_children`] but accepts any sqlx executor —
/// `&PgPool`, `&mut PgConnection`, or a transaction handle. Lets
/// tenant-scoped admin / API code use the optimized one-query form
/// against a `Tenant::conn()` connection (search_path scoped to the
/// tenant's schema), instead of falling back to a per-parent
/// `count_on` loop (N+1).
///
/// # Errors
/// As [`annotate_count_children`].
pub async fn annotate_count_children_on<'c, P, E>(
    parent_qs: crate::query::QuerySet<P>,
    child_table: &'static str,
    child_fk_column: &'static str,
    executor: E,
) -> Result<Vec<(P, i64)>, ExecError>
where
    P: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    use std::fmt::Write as _;
    let select = parent_qs.compile()?;
    let parent = select.model;
    let pk_field = parent.primary_key().ok_or(ExecError::MissingPrimaryKey {
        table: parent.table,
    })?;

    // Build the SQL by hand — the existing compile_select doesn't
    // emit GROUP BY or aggregate columns. We mirror its conventions
    // (qualified columns, $N placeholders) for consistency.
    let cols: Vec<&'static str> = parent.scalar_fields().map(|f| f.column).collect();
    let mut sql = String::from("SELECT ");
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let _ = write!(sql, "\"{}\".\"{col}\"", parent.table);
    }
    let _ = write!(
        sql,
        ", COUNT(\"{child_table}\".\"{child_pk}\") AS \"__annotated_count\" FROM \"{parent_table}\" LEFT JOIN \"{child_table}\" ON \"{child_table}\".\"{child_fk_column}\" = \"{parent_table}\".\"{parent_pk}\"",
        parent_table = parent.table,
        parent_pk = pk_field.column,
        child_pk = "id",
    );

    // Forward WHERE / ORDER BY / LIMIT / OFFSET from the parent queryset.
    let tail = crate::sql::postgres::compile_where_order_tail(
        &select.where_clause,
        select.search.as_ref(),
        &select.order_by,
        select.limit,
        select.offset,
        Some(parent.table),
        Some(parent),
    )?;

    // GROUP BY before the WHERE tail.
    sql.push_str(" GROUP BY ");
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let _ = write!(sql, "\"{}\".\"{col}\"", parent.table);
    }
    sql.push_str(&tail.sql);

    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&sql);
    for param in tail.params {
        q = bind_query(q, param);
    }
    let raw_rows = q.fetch_all(executor).await?;
    let mut out = Vec::with_capacity(raw_rows.len());
    for row in &raw_rows {
        let parent_obj = P::from_row(row)?;
        let count: i64 = sqlx::Row::try_get(row, "__annotated_count")?;
        out.push((parent_obj, count));
    }
    Ok(out)
}

/// Slice 9.0e — `prefetch_related` Django-shape: fetch a list of
/// parents and, for each one, the children that point at it via a
/// foreign key. **Two SQL queries total**, regardless of how many
/// parents:
///
/// ```text
///   SELECT * FROM <parent>;
///   SELECT * FROM <child> WHERE <fk_column> IN ($1, $2, ...);
/// ```
///
/// Returns `Vec<(Parent, Vec<Child>)>` — each parent paired with its
/// children. Parents with no matching children get an empty `Vec`.
/// The order of parents matches the queryset; the order of children
/// within each group matches the order of the second query (lex by
/// PK is the typical default; pass `.limit()` / `.offset()` on the
/// child queryset if you need to scope).
///
/// `child_fk_column` is the SQL column on the child table that
/// stores the parent's PK — for `Post { author: ForeignKey<Author> }`,
/// that's `"author"`. The function looks up child rows where
/// `<child_fk_column> IN (parent_pks)` and groups them by reading
/// the same column on each fetched child via the
/// macro-generated [`FkPkAccess`] impl.
///
/// Closes the multi-parent gap left by v0.8.2's `<parent>::<child>_set`
/// helper (which fetches one parent's children at a time, requiring
/// N queries for N parents).
///
/// # Errors
/// Anything either of the underlying `fetch` calls returns.
pub async fn fetch_with_prefetch<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    pool: &PgPool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin + LoadRelated + HasPkValue,
    C: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin + LoadRelated + FkPkAccess,
{
    let parents: Vec<P> = parent_qs.fetch(pool).await?;
    if parents.is_empty() {
        return Ok(Vec::new());
    }

    // Collect parent PKs. Models without an integer PK can't be
    // batch-prefetched this way; treat as "no children" for those
    // parents (consistent with empty-set behaviour).
    let pk_field = P::SCHEMA.primary_key().ok_or(ExecError::MissingPrimaryKey {
        table: P::SCHEMA.table,
    })?;
    let mut parent_pks: Vec<i64> = Vec::with_capacity(parents.len());
    for parent in &parents {
        if let Some(pk) = sql_value_as_i64(&extract_pk_value(parent)) {
            parent_pks.push(pk);
        }
    }
    parent_pks.sort_unstable();
    parent_pks.dedup();
    if parent_pks.is_empty() {
        return Ok(parents.into_iter().map(|p| (p, Vec::new())).collect());
    }

    // Batch-fetch the children where their FK column points at any
    // of the parent PKs.
    let pk_values: Vec<crate::core::SqlValue> = parent_pks
        .iter()
        .copied()
        .map(crate::core::SqlValue::I64)
        .collect();
    let children: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter(
            child_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(pk_values),
        )
        .fetch(pool)
        .await?;

    // Group children by FK PK.
    let mut grouped: std::collections::HashMap<i64, Vec<C>> = std::collections::HashMap::new();
    for child in children {
        let Some(fk_pk) = child.__rustango_fk_pk(child_fk_column) else {
            continue;
        };
        grouped.entry(fk_pk).or_default().push(child);
    }

    // Stitch.
    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let pk = sql_value_as_i64(&extract_pk_value(&parent)).unwrap_or(0);
        let kids = grouped.remove(&pk).unwrap_or_default();
        out.push((parent, kids));
    }
    let _ = pk_field; // suppress unused-warning when only the PK lookup ran
    Ok(out)
}

/// Extract a model's PK as a `SqlValue` via the macro-generated
/// `__rustango_pk_value`. The trait bound `LoadRelated` is satisfied
/// by every Model derive but doesn't expose `__rustango_pk_value`,
/// so we go through `sqlx::Row` instead — every Model also impls
/// `FromRow`, and we already have an instance.
///
/// Actually we have the instance; the macro emits
/// `__rustango_pk_value` as an inherent method. Calling it through
/// a trait object would force a new trait. Punt: use sqlx-side
/// extraction via `sqlx::Encode` against the schema field. Cleaner:
/// just have callers' Models implement `PrefetchableParent`.
///
/// For the v0.9 MVP we leverage the fact that every Model with a
/// PK has `__rustango_pk_value`. We add a small trait `HasPkValue`
/// that the macro impls; its body just calls the inherent method.
fn extract_pk_value<P: HasPkValue>(parent: &P) -> crate::core::SqlValue {
    parent.__rustango_pk_value_impl()
}

fn sql_value_as_i64(v: &crate::core::SqlValue) -> Option<i64> {
    match v {
        crate::core::SqlValue::I64(n) => Some(*n),
        crate::core::SqlValue::I32(n) => Some(i64::from(*n)),
        _ => None,
    }
}

/// Hidden trait — exposes the macro-generated inherent
/// `__rustango_pk_value` method polymorphically so generic
/// `fetch_with_prefetch` can read parent PKs without forcing the
/// caller to write a closure.
#[doc(hidden)]
pub trait HasPkValue {
    fn __rustango_pk_value_impl(&self) -> crate::core::SqlValue;
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
            SqlValue::Json(v) => $q.bind(sqlx::types::Json(v)),
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

// ------------------------------------------------------------------ bulk UPDATE

/// Execute a [`BulkUpdateQuery`] — `UPDATE … FROM (VALUES …)` — and return
/// the number of rows affected. One round-trip for any number of rows.
///
/// # Errors
/// SQL-writing or driver failures, or [`SqlError::EmptyBulkInsert`] if
/// `query.rows` is empty (the caller should short-circuit).
pub async fn bulk_update(
    pool: &PgPool,
    query: &BulkUpdateQuery,
) -> Result<u64, ExecError> {
    bulk_update_on(pool, query).await
}

/// Like [`bulk_update`] but accepts any sqlx executor.
///
/// # Errors
/// As [`bulk_update`].
pub async fn bulk_update_on<'c, E>(
    executor: E,
    query: &BulkUpdateQuery,
) -> Result<u64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    if query.rows.is_empty() {
        return Ok(0);
    }
    let stmt = Postgres.compile_bulk_update(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for p in stmt.params {
        q = bind_query(q, p);
    }
    Ok(q.execute(executor).await?.rows_affected())
}

// ------------------------------------------------------------------ raw SQL escape hatch

/// Execute arbitrary SQL and decode each row into `T` using the same
/// `sqlx::FromRow` impl generated by `#[derive(Model)]`.
///
/// `binds` must be in `$1` / `$2` / … placeholder order. This bypasses all
/// ORM validation and audit; use it when the query IR can't express what you
/// need (CTEs, LATERAL joins, UNNEST, window functions, etc.).
///
/// # Errors
/// Driver / SQL failures.
pub async fn raw_query<T>(
    sql: &str,
    binds: Vec<SqlValue>,
    pool: &PgPool,
) -> Result<Vec<T>, ExecError>
where
    T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    raw_query_on(sql, binds, pool).await
}

/// Like [`raw_query`] but accepts any sqlx executor.
///
/// # Errors
/// As [`raw_query`].
pub async fn raw_query_on<'c, T, E>(
    sql: &str,
    binds: Vec<SqlValue>,
    executor: E,
) -> Result<Vec<T>, ExecError>
where
    T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> = sqlx::query_as(sql);
    for b in binds {
        q = bind_query_as(q, b);
    }
    Ok(q.fetch_all(executor).await?)
}

/// Execute arbitrary SQL that does not return rows (INSERT / UPDATE / DELETE /
/// DDL). Returns the number of rows affected.
///
/// # Errors
/// Driver / SQL failures.
pub async fn raw_execute(
    sql: &str,
    binds: Vec<SqlValue>,
    pool: &PgPool,
) -> Result<u64, ExecError> {
    raw_execute_on(sql, binds, pool).await
}

/// Like [`raw_execute`] but accepts any sqlx executor.
///
/// # Errors
/// As [`raw_execute`].
pub async fn raw_execute_on<'c, E>(
    sql: &str,
    binds: Vec<SqlValue>,
    executor: E,
) -> Result<u64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
    for b in binds {
        q = bind_query(q, b);
    }
    Ok(q.execute(executor).await?.rows_affected())
}

// ------------------------------------------------------------------ aggregate

/// Execute an [`AggregateQuery`] and return each result row as a
/// `HashMap<String, SqlValue>`. Keys are the `group_by` column names and
/// the aggregate aliases from `aggregates`.
///
/// # Errors
/// SQL-writing or driver failures.
pub async fn fetch_aggregate(
    query: &AggregateQuery,
    pool: &PgPool,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError> {
    fetch_aggregate_on(query, pool).await
}

/// Like [`fetch_aggregate`] but accepts any sqlx executor.
///
/// # Errors
/// As [`fetch_aggregate`].
pub async fn fetch_aggregate_on<'c, E>(
    query: &AggregateQuery,
    executor: E,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_aggregate(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for p in stmt.params {
        q = bind_query(q, p);
    }
    let raw_rows = q.fetch_all(executor).await?;

    // Collect result column names from the first row's columns.
    let mut out = Vec::with_capacity(raw_rows.len());
    for row in &raw_rows {
        use sqlx::{Column as _, Row as _};
        let mut map = std::collections::HashMap::new();
        for (i, col) in row.columns().iter().enumerate() {
            let name = col.name().to_owned();
            // Try to decode as each possible SqlValue type, falling back to Null.
            let val: SqlValue = if let Ok(v) = row.try_get::<i64, _>(i) {
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
            };
            map.insert(name, val);
        }
        out.push(map);
    }
    Ok(out)
}

// ------------------------------------------------------------------ transaction

/// Run `f` inside a Postgres transaction. Commits on `Ok`, rolls back on `Err`.
///
/// Every `_on(executor)` method accepts `&mut PgConnection`, which is what the
/// closure receives — so all ORM writes can be composed inside a single atomic
/// block with no extra boilerplate:
///
/// ```ignore
/// rustango::sql::transaction(&pool, |conn| async move {
///     user.save_on(conn).await?;
///     post.save_on(conn).await?;
///     Ok(())
/// }).await?;
/// ```
///
/// # Errors
/// Returns the first `ExecError` produced by `f`, or a driver error if
/// `BEGIN` / `COMMIT` / `ROLLBACK` fails.
pub async fn transaction<F, Fut, T>(pool: &PgPool, f: F) -> Result<T, ExecError>
where
    F: FnOnce(&mut sqlx::PgConnection) -> Fut,
    Fut: std::future::Future<Output = Result<T, ExecError>>,
{
    let mut tx = pool.begin().await?;
    match f(&mut *tx).await {
        Ok(val) => {
            tx.commit().await?;
            Ok(val)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}
