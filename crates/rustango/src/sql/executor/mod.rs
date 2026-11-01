//! Async executor — binds a `CompiledStatement` to sqlx and runs it.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, CountQuery, DeleteQuery, InsertQuery, Model,
    SelectQuery, SqlValue, UpdateQuery,
};
use crate::query::{QuerySet, UpdateBuilder};

// PG-typed helpers below import these. Sqlite/MySQL-only builds only
// see the bi/tri-dialect `_pool` entry points from this module.
#[cfg(feature = "postgres")]
use sqlx::postgres::{PgArguments, PgPool, PgRow};
#[cfg(feature = "postgres")]
use sqlx::query::{Query, QueryAs};

use super::Dialect;
use super::ExecError;
#[cfg(feature = "postgres")]
use super::Postgres;

/// Reads the PK stored in a model's `ForeignKey` fields by name, so
/// prefetch can group children under the right parent. The `Model`
/// derive implements it; models with no FK fields get a no-op impl.
#[doc(hidden)]
pub trait FkPkAccess {
    /// Read the i64 PK in a `ForeignKey<T>` field by name.
    /// `None` for unknown or non-FK fields, or a non-i64 PK type
    /// (use [`Self::__rustango_fk_pk_value`] for those).
    fn __rustango_fk_pk(&self, field_name: &str) -> Option<i64>;

    /// Read the PK in a `ForeignKey<T, K>` field by name as a
    /// [`crate::core::SqlValue`]. Works for any PK type.
    /// `None` for unknown or non-FK fields.
    fn __rustango_fk_pk_value(&self, field_name: &str) -> Option<crate::core::SqlValue>;
}

/// Lets generic `fetch_on` code call a model's per-FK
/// `select_related` loaders. The `Model` derive implements it;
/// models with no FK fields get a no-op impl, so the bound on
/// `fetch_on` is always satisfied.
#[doc(hidden)]
#[cfg(feature = "postgres")]
pub trait LoadRelated {
    /// Stitch a `select_related`-loaded parent onto this instance's
    /// FK field. `field_name` is the FK field's Rust name (e.g.
    /// `"author"`); `alias` is the SELECT writer's alias prefix for
    /// that JOIN's columns. Unknown field names return `Ok(false)`,
    /// so directives that don't apply are skipped.
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

/// Always-on marker when `postgres` is off — the macro emits an
/// empty impl unconditionally so generic bounds resolve.
#[cfg(not(feature = "postgres"))]
#[doc(hidden)]
pub trait LoadRelated {}
#[cfg(not(feature = "postgres"))]
impl<T> LoadRelated for T {}

/// Reduce `select_related` join aliases to the leaf ones — those that
/// no longer alias extends at a `__` boundary — each paired with its
/// first hop. One join is emitted per hop (`author`, `author__profile`),
/// but `__rustango_load_related` decodes the whole chain from the leaf,
/// so stitching only needs the leaves. A single-hop alias yields
/// `(alias, alias)`.
pub(crate) fn select_related_leaves(aliases: &[&'static str]) -> Vec<(&'static str, &'static str)> {
    aliases
        .iter()
        .copied()
        .filter(|a| {
            // Keep `a` only if no other alias extends it as `a__…`.
            !aliases.iter().any(|b| {
                *b != *a
                    && b.len() > a.len()
                    && b.starts_with(a)
                    && b.as_bytes()[a.len()..].starts_with(b"__")
            })
        })
        .map(|a| {
            let first_hop = a.split_once("__").map(|(h, _)| h).unwrap_or(a);
            (a, first_hop)
        })
        .collect()
}

/// Run `fetch` once per slice of `keys` that fits one `IN` list beside the
/// binds `base` already carries, and concatenate the rows (#2295).
/// `fetch` must add exactly one `IN` list over its keys to `base`.
pub(crate) async fn fetch_in_chunks<T, C, F, Fut>(
    pool: &Pool,
    base: &QuerySet<T>,
    keys: Vec<SqlValue>,
    fetch: F,
) -> Result<Chunked<C>, ExecError>
where
    T: Model,
    F: FnMut(Vec<SqlValue>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<C>, ExecError>>,
{
    let select = base.clone().compile()?;
    let batches = in_batches(pool.dialect(), &select, base.is_sliced(), keys)?;
    let split = batches.len() > 1;
    let rows = fetch_batches(batches, fetch).await?;
    Ok(Chunked { rows, split })
}

/// Rows [`fetch_in_chunks`] read over one or more `IN` lists. A collation
/// can fold keys in two lists onto one row, so split results may repeat it.
pub(crate) struct Chunked<C> {
    rows: Vec<C>,
    split: bool,
}

impl<C> Chunked<C> {
    /// The rows as read: for callers keying them so a repeat collapses,
    /// or whose keys no collation folds.
    pub(crate) fn rows(self) -> Vec<C> {
        self.rows
    }
}

impl<C: HasPkValue> Chunked<C> {
    /// Each row once, by primary key.
    pub(crate) fn distinct(self) -> Vec<C> {
        if self.split {
            __rustango_dedupe_by_pk(self.rows)
        } else {
            self.rows
        }
    }
}

/// [`fetch_in_chunks`] over a schema-level `base`, for callers with no
/// typed model (#2318). Rows are not deduped.
pub(crate) async fn fetch_select_in_chunks<C, F, Fut>(
    pool: &Pool,
    base: &SelectQuery,
    sliced: bool,
    keys: Vec<SqlValue>,
    fetch: F,
) -> Result<Vec<C>, ExecError>
where
    F: FnMut(Vec<SqlValue>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<C>, ExecError>>,
{
    fetch_batches(in_batches(pool.dialect(), base, sliced, keys)?, fetch).await
}

async fn fetch_batches<C, F, Fut>(
    batches: Vec<Vec<SqlValue>>,
    mut fetch: F,
) -> Result<Vec<C>, ExecError>
where
    F: FnMut(Vec<SqlValue>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<C>, ExecError>>,
{
    let mut out = Vec::new();
    for batch in batches {
        out.extend(fetch(batch).await?);
    }
    Ok(out)
}

/// Split `keys` into the `IN` lists `base` has bind room for. Exact
/// duplicates are dropped; keys a collation folds together are not, so
/// callers dedupe split results by row.
fn in_batches(
    dialect: &dyn Dialect,
    base: &SelectQuery,
    sliced: bool,
    keys: Vec<SqlValue>,
) -> Result<Vec<Vec<SqlValue>>, ExecError> {
    let room = bind_room(dialect, base)?;
    // One `= ANY($n)` bind holds the whole list where arrays bind.
    if room > 0 && crate::sql::writers::in_list_binds(dialect, &keys) <= room {
        return Ok(vec![keys]);
    }
    let mut seen = std::collections::HashSet::new();
    let keys: Vec<SqlValue> = keys
        .into_iter()
        .filter(|k| seen.insert(dedupe_key(k)))
        .collect();
    if room > 0 && crate::sql::writers::in_list_binds(dialect, &keys) <= room {
        return Ok(vec![keys]);
    }
    let size = in_chunk_size(base.model.table, keys.len(), room, sliced)?;
    Ok(keys.chunks(size).map(<[SqlValue]>::to_vec).collect())
}

/// One key per distinct value: `I32(1)` and `I64(1)` name the same row.
fn dedupe_key(k: &SqlValue) -> String {
    match k {
        SqlValue::I16(n) => format!("{:?}", SqlValue::I64(i64::from(*n))),
        SqlValue::I32(n) => format!("{:?}", SqlValue::I64(i64::from(*n))),
        other => format!("{other:?}"),
    }
}

/// `keys` as the `IN` lists one statement over `T` can bind, for the
/// generated `find_many` / `where_in` / `destroy` shortcuts.
///
/// # Errors
/// [`ExecError::InListUnsplittable`] when `T`'s scopes fill the bind cap.
#[doc(hidden)]
pub fn __rustango_in_batches<T: Model>(
    pool: &Pool,
    keys: Vec<SqlValue>,
) -> Result<Vec<Vec<SqlValue>>, ExecError> {
    let base = QuerySet::<T>::default().compile()?;
    in_batches(pool.dialect(), &base, false, keys)
}

/// `keys` as the one `NOT IN` list a statement over `T` can bind: exact
/// duplicates dropped. A `NOT IN` cannot be split across statements.
///
/// # Errors
/// [`ExecError::InListUnsplittable`] when the keys still pass the bind cap.
#[doc(hidden)]
pub fn __rustango_one_in_list<T: Model>(
    pool: &Pool,
    keys: Vec<SqlValue>,
) -> Result<Vec<SqlValue>, ExecError> {
    let dialect = pool.dialect();
    let room = bind_room(dialect, &QuerySet::<T>::default().compile()?)?;
    let mut seen = std::collections::HashSet::new();
    let keys: Vec<SqlValue> = keys
        .into_iter()
        .filter(|k| seen.insert(dedupe_key(k)))
        .collect();
    if crate::sql::writers::in_list_binds(dialect, &keys) > room {
        return Err(ExecError::InListUnsplittable {
            table: T::SCHEMA.table,
            keys: keys.len(),
            max: room,
        });
    }
    Ok(keys)
}

/// `rows` fetched over split `IN` lists, one per primary key: a collation
/// can match one row from keys in two batches.
#[doc(hidden)]
#[must_use]
pub fn __rustango_dedupe_by_pk<T: HasPkValue>(rows: Vec<T>) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|r| seen.insert(dedupe_key(&r.__rustango_pk_value_impl())))
        .collect()
}

/// Binds left for an `IN` list beside the ones `base` already carries.
fn bind_room(dialect: &dyn Dialect, base: &SelectQuery) -> Result<usize, ExecError> {
    let carried = dialect.compile_select(base)?.params.len();
    Ok(dialect.max_bind_params().saturating_sub(carried))
}

/// Keys per `IN` list: the `room` the query's own binds leave. Refused when
/// there is none, or when a limit or offset would apply per batch.
fn in_chunk_size(
    table: &'static str,
    keys: usize,
    room: usize,
    sliced: bool,
) -> Result<usize, ExecError> {
    if room == 0 || (sliced && keys > room) {
        return Err(ExecError::InListUnsplittable {
            table,
            keys,
            max: room,
        });
    }
    Ok(room)
}

/// Fails when the `select_related` LEFT JOIN under `alias` matched no row
/// though the FK is set: the target's PK cell is NULL, so the row is missing.
///
/// # Errors
/// `sqlx::Error::ColumnDecode` for a missing target row, or a missing PK column.
#[doc(hidden)]
pub fn __rustango_require_join<R>(
    row: &R,
    target: &crate::core::ModelSchema,
    alias: &str,
) -> Result<(), sqlx::Error>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
{
    let Some(pk) = target.primary_key() else {
        return Ok(());
    };
    let col = crate::core::joins::joined_label(alias, pk.column);
    if sqlx::ValueRef::is_null(&row.try_get_raw(col.as_str())?) {
        return Err(sqlx::Error::ColumnDecode {
            index: col,
            source: format!("foreign-key target row in `{}` is missing", target.table).into(),
        });
    }
    Ok(())
}

#[cfg(feature = "postgres")]
impl<T> QuerySet<T>
where
    T: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    /// [`Self::fetch_paginated_on`] against a pool, for non-tenant code.
    ///
    /// # Errors
    /// As [`Self::fetch_paginated_on`].
    pub async fn fetch_paginated(self, pool: &PgPool) -> Result<Page<T>, ExecError> {
        let select = self.compile()?;
        let stmt = paginated_statement(&Postgres, &select)?;
        // Its own decode only for the looser `FromRow<PgRow>` bound.
        let rows = run_on(pool, &stmt.sql, stmt.params, |row| match row {
            RowRef::Postgres(r) => paged::<T, _>(r),
            #[allow(unreachable_patterns)]
            _ => unreachable!("a PgPool yields Postgres rows"),
        })
        .await?;
        let total = match total_statement(&Postgres, &select)?.filter(|_| rows.is_empty()) {
            Some(count) => Some(count_stmt_on(pool, count).await?),
            None => None,
        };
        Ok(into_page(rows, total))
    }
}

/// One `fetch_paginated` row and its window total.
fn paged<T, R>(row: &R) -> Result<(T, i64), sqlx::Error>
where
    R: sqlx::Row,
    T: for<'r> sqlx::FromRow<'r, R>,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    i64: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    Ok((T::from_row(row)?, row.try_get("__rustango_total")?))
}

/// An empty page has no row to carry the total: `counted` is it, or 0.
fn into_page<T>(rows: Vec<(T, i64)>, counted: Option<i64>) -> Page<T> {
    let total = rows
        .first()
        .map_or(counted.unwrap_or(0), |(_, total)| *total);
    Page {
        rows: rows.into_iter().map(|(row, _)| row).collect(),
        total,
    }
}

impl<T> QuerySet<T>
where
    T: Model + MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    /// Fetch a page of rows **and** the total matching count in one
    /// round trip. `COUNT(*) OVER ()` returns the pre-LIMIT total on
    /// every row, so there is no second `SELECT COUNT(*)`.
    ///
    /// ```ignore
    /// let page: Page<Post> = Post::objects()
    ///     .where_(Post::published.eq(true))
    ///     .limit(20).offset(40)
    ///     .fetch_paginated_on(&mut conn).await?;
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
    /// A page past the last row runs one `COUNT(*)` for its total (not
    /// on a bare sqlx Postgres executor, where it stays 0).
    /// MySQL needs 8.0 or later for the window function.
    ///
    /// # Errors
    /// As [`Self::fetch_on`].
    pub async fn fetch_paginated_on<'c, E: SqlExecutor<'c>>(
        self,
        exec: E,
    ) -> Result<Page<T>, ExecError> {
        let select = self.compile()?;
        let stmt = paginated_statement(exec.dialect(), &select)?;
        let count = total_statement(exec.dialect(), &select)?;
        let (rows, total) = exec
            .fetch_rows_or_count(stmt, count, |row| match row {
                #[cfg(feature = "postgres")]
                RowRef::Postgres(r) => paged::<T, _>(r),
                #[cfg(feature = "mysql")]
                RowRef::Mysql(r) => paged::<T, _>(r),
                #[cfg(feature = "sqlite")]
                RowRef::Sqlite(r) => paged::<T, _>(r),
            })
            .await?;
        Ok(into_page(rows, total))
    }

    /// [`QuerySet::in_bulk`] on a [`SqlExecutor`], e.g. `Tenant::conn()`
    /// whose connection holds the tenant's `search_path`.
    ///
    /// # Errors
    /// As [`Self::fetch_on`]; [`ExecError::InListUnsplittable`] when `ids`
    /// exceed one `IN` list, since one executor runs one statement.
    pub async fn in_bulk_on<'c, E, C, K, I, F>(
        self,
        column: C,
        ids: I,
        extract: F,
        executor: E,
    ) -> Result<std::collections::HashMap<K, T>, ExecError>
    where
        E: SqlExecutor<'c>,
        T: LoadRelated + MaybeMyLoadRelated + MaybeSqliteLoadRelated,
        C: crate::core::Column<Model = T>,
        K: Eq + std::hash::Hash + Into<crate::core::SqlValue>,
        I: IntoIterator<Item = K>,
        F: Fn(&T) -> K,
    {
        let _ = column;
        let id_values: Vec<crate::core::SqlValue> = ids.into_iter().map(|v| v.into()).collect();
        if id_values.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let room = bind_room(executor.dialect(), &self.clone().compile()?)?;
        if crate::sql::writers::in_list_binds(executor.dialect(), &id_values) > room {
            return Err(ExecError::InListUnsplittable {
                table: T::SCHEMA.table,
                keys: id_values.len(),
                max: room,
            });
        }
        let rows = self
            .filter_op(
                C::NAME,
                crate::core::Op::In,
                crate::core::SqlValue::List(id_values),
            )
            .fetch_on(executor)
            .await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let key = extract(&row);
            out.insert(key, row);
        }
        Ok(out)
    }
}

mod page;
pub use page::Page;
use page::{paginated_statement, total_statement};

// PG `_on` CRUD family — see pg_on.rs.
#[cfg(feature = "postgres")]
mod pg_on;
#[cfg(feature = "postgres")]
pub use pg_on::{
    bulk_insert_on, delete_on, insert_on, insert_returning_on, select_rows_on, update_on,
};

mod row_to_json;
#[cfg(feature = "postgres")]
pub use row_to_json::row_to_json;
#[cfg(feature = "mysql")]
pub use row_to_json::row_to_json_my;
#[cfg(feature = "sqlite")]
pub use row_to_json::row_to_json_sqlite;
pub(crate) use row_to_json::select_rows_as_json_with;
pub use row_to_json::{select_one_row_as_json, select_rows_as_json};
#[cfg(feature = "admin")]
pub(crate) use row_to_json::{select_one_row_as_json_tx, select_rows_as_json_tx};

/// Annotate each parent row with the COUNT of its children, from a
/// single query, so a list page costs one round trip instead of
/// N + 1:
///
/// ```text
///   SELECT parent.<every-column>,
///     (SELECT COUNT(*) FROM child WHERE child.<fk_column> = parent.<pk>)
///       AS __annotated_count
///   FROM parent [WHERE / ORDER BY / LIMIT from `parent_qs`]
/// ```
///
/// Handles one Count over one reverse-FK relation. For `Sum`, `Avg`
/// and friends use the aggregate query builder.
///
/// `child_table` is the child model's table; `child_fk_column` is the
/// column on it that holds the parent's PK.
///
/// # Errors
/// SQL-writing or driver failures from the single SELECT.
#[cfg(feature = "postgres")]
pub async fn annotate_count_children<P>(
    parent_qs: crate::query::QuerySet<P>,
    child_table: &'static str,
    child_fk_column: &'static str,
    pool: &PgPool,
) -> Result<Vec<(P, i64)>, ExecError>
where
    P: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    let stmt = annotate_count_statement(&Postgres, parent_qs, child_table, child_fk_column)?;
    // Its own decode only for the looser `FromRow<PgRow>` bound.
    run_on(pool, &stmt.sql, stmt.params, |row| match row {
        RowRef::Postgres(r) => counted::<P, _>(r),
        #[allow(unreachable_patterns)]
        _ => unreachable!("a PgPool yields Postgres rows"),
    })
    .await
}

/// [`annotate_count_children`] on a [`SqlExecutor`], e.g. `Tenant::conn()`
/// whose connection holds the tenant's `search_path`.
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
    P: Model + MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
    E: SqlExecutor<'c>,
{
    let stmt =
        annotate_count_statement(executor.dialect(), parent_qs, child_table, child_fk_column)?;
    run_on(executor, &stmt.sql, stmt.params, |row| match row {
        #[cfg(feature = "postgres")]
        RowRef::Postgres(r) => counted::<P, _>(r),
        #[cfg(feature = "mysql")]
        RowRef::Mysql(r) => counted::<P, _>(r),
        #[cfg(feature = "sqlite")]
        RowRef::Sqlite(r) => counted::<P, _>(r),
    })
    .await
}

/// One `annotate_count_children` row: the parent and its child count.
fn counted<P, R>(row: &R) -> Result<(P, i64), sqlx::Error>
where
    R: sqlx::Row,
    P: for<'r> sqlx::FromRow<'r, R>,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    i64: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    Ok((P::from_row(row)?, row.try_get("__annotated_count")?))
}

/// The `annotate_count_children` statement for dialect `d`.
fn annotate_count_statement<P: Model>(
    d: &dyn Dialect,
    parent_qs: crate::query::QuerySet<P>,
    child_table: &'static str,
    child_fk_column: &'static str,
) -> Result<super::CompiledStatement, ExecError> {
    let select = parent_qs.compile()?;
    let pk_field = select
        .model
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: select.model.table,
        })?;
    let child = super::writers::ChildCount {
        child_table,
        fk_column: child_fk_column,
        parent_pk: pk_field.column,
    };
    Ok(super::writers::compile_select_with_child_count(
        d, &select, child,
    )?)
}

/// Prefetch children: fetch parents and, for each one,
/// the children whose foreign key points at it. Two queries in total,
/// however many parents there are:
///
/// ```text
///   SELECT * FROM <parent>;
///   SELECT * FROM <child> WHERE <fk_column> IN ($1, $2, ...);
/// ```
///
/// Each parent is paired with its children; a parent with no children
/// gets an empty `Vec`. Parents keep the queryset's order, children
/// keep the second query's order.
///
/// `child_fk_column` is the column on the child table that holds the
/// parent's PK — for `Post { author: ForeignKey<Author> }` that is
/// `"author"`. Children are grouped by reading the same column back
/// through [`FkPkAccess`].
///
/// # Errors
/// Anything either of the underlying `fetch` calls returns.
#[cfg(feature = "postgres")]
pub async fn fetch_with_prefetch<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    pool: &PgPool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model
        + for<'r> sqlx::FromRow<'r, PgRow>
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + Send
        + Unpin
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue,
    C: Model
        + for<'r> sqlx::FromRow<'r, PgRow>
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + Send
        + Unpin
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + FkPkAccess,
{
    // The pool path batches the `IN` list past the bind cap.
    fetch_with_prefetch_pool(parent_qs, child_fk_column, &Pool::Postgres(pool.clone())).await
}

/// Read a model's PK as a [`crate::core::SqlValue`] through
/// [`HasPkValue`].
pub(super) fn extract_pk_value<P: HasPkValue>(parent: &P) -> crate::core::SqlValue {
    parent.__rustango_pk_value_impl()
}

/// Exposes a model's `__rustango_pk_value` to generic code, so
/// `fetch_with_prefetch` can read parent PKs without a caller closure.
#[doc(hidden)]
pub trait HasPkValue {
    fn __rustango_pk_value_impl(&self) -> crate::core::SqlValue;

    /// The value in SQL column `column`, for a child FK `on` a non-pk
    /// column. `None` for an unknown column.
    fn __rustango_column_value(&self, column: &str) -> Option<crate::core::SqlValue> {
        let _ = column;
        None
    }
}

#[cfg(feature = "postgres")]
impl<T: Model + Send> QuerySet<T> {
    /// Run `EXPLAIN` on this queryset and return the plan, one line
    /// per `Vec` entry. Runs against a pool with the default options:
    /// plain `EXPLAIN`, no `ANALYZE`, so the query itself is not run.
    /// Use [`Self::explain_on`] to pick the executor and options.
    ///
    /// ```ignore
    /// use rustango::sql::ExplainOptions;
    /// let plan = Post::objects()
    ///     .where_(Post::author_id.eq(7_i64))
    ///     .explain(&pool)
    ///     .await?;
    /// for line in plan { println!("{line}"); }
    /// ```
    ///
    /// # Errors
    /// SQL-writing or driver failures from the EXPLAIN.
    pub async fn explain(self, pool: &PgPool) -> Result<Vec<String>, ExecError> {
        self.explain_on(pool, ExplainOptions::default()).await
    }
}

impl<T: Model + Send> QuerySet<T> {
    /// [`Self::explain`] with your own [`SqlExecutor`] and [`ExplainOptions`],
    /// on any backend (see [`explain_pool`] for each one's syntax).
    /// `analyze = true` really runs the query, side effects included,
    /// so it is opt-in.
    ///
    /// # Errors
    /// SQL-writing or driver failures from the EXPLAIN.
    pub async fn explain_on<'c, E: SqlExecutor<'c>>(
        self,
        executor: E,
        options: ExplainOptions,
    ) -> Result<Vec<String>, ExecError> {
        explain::explain_lines_on(executor, &self.compile()?, &options).await
    }
}

mod explain;
pub use explain::{explain_pool, ExplainFormat, ExplainOptions};

impl<T: Model + Send> QuerySet<T> {
    /// Bulk-DELETE every row matching this queryset's filters, on a
    /// [`SqlExecutor`] like [`Self::fetch_on`]. Returns the number of
    /// rows affected.
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    pub async fn delete_on<'c, E: SqlExecutor<'c>>(self, executor: E) -> Result<u64, ExecError> {
        refuse_audited::<T>("`delete_on` cannot audit; use `Model::delete_where`")?;
        let query = self.compile_delete()?;
        let plan = crate::sql::compile_delete_plan(executor.dialect(), &query)?;
        // A raw sqlx executor runs one statement: delete as before, unaudited cascade.
        let single = plan.links.is_empty() && !executor.runs_plans();
        if plan.is_one_statement() || single {
            if plan.cascade.is_some() {
                crate::audit::cascade::warn_unaudited(query.model.table);
            }
            return write_on(executor, &plan.owner.sql, plan.owner.params).await;
        }
        executor.run_delete_plan(plan).await
    }
}

impl<T: Model + Send> UpdateBuilder<T> {
    /// Compile and run this `UpdateBuilder` on a [`SqlExecutor`].
    /// Returns the number of rows affected.
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    pub async fn execute_on<'c, E: SqlExecutor<'c>>(self, executor: E) -> Result<u64, ExecError> {
        refuse_audited::<T>("`execute_on` cannot audit; use `execute_pool`")?;
        let query = self.compile()?;
        query.validate()?;
        let stmt = executor.dialect().compile_update(&query)?;
        write_on(executor, &stmt.sql, stmt.params).await
    }
}

/// Gives `UpdateBuilder` an `execute_pool(&Pool)` method that runs on
/// any backend, via [`update_pool`].
///
/// Import with `use rustango::sql::UpdaterPool;`.
pub trait UpdaterPool<T: Model + Send> {
    /// Compile and run the update against `pool`. Returns the number
    /// of rows affected.
    ///
    /// # Errors
    /// [`ExecError`] for schema, SQL-writing, or driver failures.
    fn execute_pool(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<u64, ExecError>> + Send;
}

impl<T: Model + Send> UpdaterPool<T> for UpdateBuilder<T> {
    async fn execute_pool(self, pool: &Pool) -> Result<u64, ExecError> {
        let query = self.compile()?;
        // Audited models audit each updated row (#1747).
        if let Some(run) = T::__rustango_audited_update() {
            return run(pool, &query, crate::audit::AuditOp::Update).await;
        }
        update_pool(pool, &query).await
    }
}

/// Refuse a write that cannot audit on an audited model.
fn refuse_audited<T: Model>(reason: &'static str) -> Result<(), ExecError> {
    if T::SCHEMA.audit_track.is_some() {
        return Err(ExecError::AuditUnsupported {
            table: T::SCHEMA.table,
            reason,
        });
    }
    Ok(())
}

/// A NULL with no type attached, so PostgreSQL infers the type from
/// the column it lands in.
///
/// Binding `None::<String>` would send the text OID, and Postgres then
/// rejects the parameter for any non-text column. OID 0 is the wire
/// protocol's "unspecified", so the server resolves the type during
/// Parse. MySQL and SQLite type parameters loosely and don't need this.
#[cfg(feature = "postgres")]
struct UntypedNull;

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for UntypedNull {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_oid(sqlx::postgres::types::Oid(0))
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Encode<'_, sqlx::Postgres> for UntypedNull {
    fn encode_by_ref(
        &self,
        _buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        Ok(sqlx::encode::IsNull::Yes)
    }
}

/// Match on `SqlValue` and bind it to a sqlx query builder. Used for
/// both `Query` and `QueryAs`, which share no bind trait.
///
/// PG-only: it relies on `sqlx::types::Json` through `PgArguments` and
/// on `SqlValue::Array` binding as a typed PG array. Use
/// `bind_match_mysql!` / `bind_match_sqlite!` for the other dialects.
#[cfg(feature = "postgres")]
macro_rules! bind_match {
    ($q:expr, $value:expr) => {
        match $value {
            SqlValue::Null => $q.bind($crate::sql::executor::UntypedNull),
            SqlValue::I16(v) => $q.bind(v),
            SqlValue::I32(v) => $q.bind(v),
            SqlValue::I64(v) => $q.bind(v),
            SqlValue::F32(v) => $q.bind(v),
            SqlValue::F64(v) => $q.bind(v),
            SqlValue::Bool(v) => $q.bind(v),
            SqlValue::String(v) => $q.bind(v),
            SqlValue::DateTime(v) => $q.bind(v),
            SqlValue::Date(v) => $q.bind(v),
            SqlValue::Time(v) => $q.bind(v),
            SqlValue::Uuid(v) => $q.bind(v),
            SqlValue::Json(v) => $q.bind(sqlx::types::Json(v)),
            SqlValue::Decimal(v) => $q.bind(v),
            SqlValue::Binary(v) => $q.bind(v),
            SqlValue::List(_) => {
                unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
            }
            // Range literal: bound as text, cast by PG to the column's
            // range type.
            SqlValue::RangeLiteral(s) => $q.bind(s),
            // hstore: bound as a native `PgHstore`, no text escaping.
            SqlValue::HStore(pairs) => {
                $q.bind(sqlx::postgres::types::PgHstore(pairs.into_iter().collect()))
            }
            // pgvector: binary wire format via the `Vector` newtype.
            SqlValue::Vector(v) => $q.bind(crate::sql::Vector(v)),
            // PostGIS: EWKB binary via the `Point` newtype.
            SqlValue::Geometry { x, y, srid } => {
                $q.bind(crate::sql::Point { x, y, srid })
            }
            // PG arrays are typed, so elements must be homogeneous.
            // I16/I32/I64/String/Bool/Uuid are supported; anything else panics
            // at bind time.
            SqlValue::Array(elems) => match elems.first() {
                None => $q.bind(Vec::<i32>::new()),
                Some(SqlValue::I16(_)) => {
                    let v: Vec<i16> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::I16(n) = e { Some(n) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::Uuid(_)) => {
                    let v: Vec<::uuid::Uuid> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::Uuid(u) = e { Some(u) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::I64(_)) => {
                    let v: Vec<i64> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::I64(n) = e { Some(n) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::I32(_)) => {
                    let v: Vec<i32> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::I32(n) = e { Some(n) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::String(_)) => {
                    let v: Vec<String> = elems
                        .into_iter()
                        .filter_map(|e| {
                            if let SqlValue::String(s) = e {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::Bool(_)) => {
                    let v: Vec<bool> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::Bool(b) = e { Some(b) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(_) => unreachable!(
                    "SqlValue::Array elements other than I16/I32/I64/String/Bool/Uuid are not yet supported"
                ),
            },
        }
    };
}

/// MySQL counterpart of [`bind_match`]. MySQL has no array type, so
/// the `Array` arm is `unreachable!()`: the writer rejects array
/// operators before any bind happens.
#[cfg(feature = "mysql")]
macro_rules! bind_match_mysql {
    ($q:expr, $value:expr) => {
        match $value {
            SqlValue::Null => $q.bind(None::<String>),
            SqlValue::I16(v) => $q.bind(v),
            SqlValue::I32(v) => $q.bind(v),
            SqlValue::I64(v) => $q.bind(v),
            SqlValue::F32(v) => $q.bind(v),
            SqlValue::F64(v) => $q.bind(v),
            SqlValue::Bool(v) => $q.bind(v),
            SqlValue::String(v) => $q.bind(v),
            SqlValue::DateTime(v) => $q.bind(v),
            SqlValue::Date(v) => $q.bind(v),
            SqlValue::Time(v) => $q.bind(v),
            // The column is CHAR(36); sqlx would send `Uuid` as 16 raw bytes (#1733).
            SqlValue::Uuid(v) => $q.bind(v.hyphenated()),
            SqlValue::Json(v) => $q.bind(sqlx::types::Json(v)),
            SqlValue::Decimal(v) => $q.bind(v),
            SqlValue::Binary(v) => $q.bind(v),
            SqlValue::List(_) => {
                unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
            }
            SqlValue::Array(_) => unreachable!(
                "MySQL has no array type; `write_array_op` rejects before bind. Issue #30."
            ),
            SqlValue::RangeLiteral(_) => unreachable!(
                "MySQL has no range type; `write_range_op` rejects before bind. Issue #31."
            ),
            SqlValue::HStore(_) => {
                unreachable!("MySQL has no hstore type; `HStore` columns are PG-only. Issue #342.")
            }
            SqlValue::Vector(_) => unreachable!(
                "MySQL has no vector type; pgvector distance operators are rejected before bind. Issue #824."
            ),
            SqlValue::Geometry { .. } => unreachable!(
                "MySQL has no geometry type; PostGIS `Point` columns are PG-only. Issue #443."
            ),
        }
    };
}

/// SQLite counterpart of [`bind_match`]. `sqlx-sqlite` has no
/// `Decimal: Type<Sqlite>` impl, so the `Decimal` arm binds
/// `to_string()`. NUMERIC affinity stores it as INTEGER or REAL, so
/// digits past what a REAL holds (~15) are lost. The writer wraps the
/// placeholder in `CAST(? AS NUMERIC)` so expressions compare it as a number.
#[cfg(feature = "sqlite")]
macro_rules! bind_match_sqlite {
    ($q:expr, $value:expr) => {
        match $value {
            SqlValue::Null => $q.bind(None::<String>),
            SqlValue::I16(v) => $q.bind(v),
            SqlValue::I32(v) => $q.bind(v),
            SqlValue::I64(v) => $q.bind(v),
            SqlValue::F32(v) => $q.bind(v),
            SqlValue::F64(v) => $q.bind(v),
            SqlValue::Bool(v) => $q.bind(v),
            SqlValue::String(v) => $q.bind(v),
            // Encoded here, not by sqlx. SQLite stores datetimes as
            // TEXT and compares them as text, so the width must be
            // fixed. sqlx emits 0/3/6/9 fractional digits depending on
            // the value, so a stored timestamp would not equal its own
            // re-bound form. This matches what the DDL default writes.
            SqlValue::DateTime(v) => $q.bind(crate::sql::encode_datetime(v)),
            SqlValue::Date(v) => $q.bind(v),
            SqlValue::Time(v) => $q.bind(v),
            SqlValue::Uuid(v) => $q.bind(v),
            SqlValue::Json(v) => $q.bind(sqlx::types::Json(v)),
            // sqlite-only string round-trip — see macro doc.
            SqlValue::Decimal(v) => $q.bind(v.to_string()),
            SqlValue::Binary(v) => $q.bind(v),
            SqlValue::List(_) => {
                unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
            }
            SqlValue::Array(_) => unreachable!(
                "SQLite has no array type; `write_array_op` rejects before bind. Issue #30."
            ),
            SqlValue::RangeLiteral(_) => unreachable!(
                "SQLite has no range type; `write_range_op` rejects before bind. Issue #31."
            ),
            SqlValue::HStore(_) => {
                unreachable!("SQLite has no hstore type; `HStore` columns are PG-only. Issue #342.")
            }
            SqlValue::Vector(_) => unreachable!(
                "SQLite has no vector type; pgvector distance operators are rejected before bind. Issue #824."
            ),
            SqlValue::Geometry { .. } => unreachable!(
                "SQLite has no geometry type; PostGIS `Point` columns are PG-only. Issue #443."
            ),
        }
    };
}

#[cfg(feature = "postgres")]
pub(super) fn bind_query_as<T>(
    q: QueryAs<'_, sqlx::Postgres, T, PgArguments>,
    value: SqlValue,
) -> QueryAs<'_, sqlx::Postgres, T, PgArguments> {
    bind_match!(q, value)
}

#[cfg(feature = "postgres")]
pub(crate) fn bind_query(
    q: Query<'_, sqlx::Postgres, PgArguments>,
    value: SqlValue,
) -> Query<'_, sqlx::Postgres, PgArguments> {
    bind_match!(q, value)
}

/// The bound query of a PG `_on` read. The one place that family is
/// counted for `assert_num_queries` (#1561).
#[cfg(feature = "postgres")]
pub(super) fn pg_on_query(
    sql: &str,
    params: Vec<SqlValue>,
) -> Query<'_, sqlx::Postgres, PgArguments> {
    crate::test_assertions::query_counter::bump();
    params.into_iter().fold(sqlx::query(sql), bind_query)
}

/// Like [`fetch_aggregate_dict`] but on any [`SqlExecutor`].
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn fetch_aggregate_on<'c, E>(
    query: &AggregateQuery,
    executor: E,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError>
where
    E: SqlExecutor<'c>,
{
    values::fetch_aggregate_dict_on(executor, query).await
}

// Result cells to `SqlValue`, one plan per result set (#2655).
mod decode;

mod tx;
pub use tx::{transaction_pool, write_transaction, PoolTx};

// One executor for every `_on` method.
mod on;
use on::RowRef;
pub use on::{PoolConn, SqlExecutor};

mod atomic;
#[cfg(feature = "jobs")]
pub(crate) use atomic::in_any_block;
#[cfg(feature = "jobs-postgres")]
pub(crate) use atomic::in_block;
pub(crate) use atomic::rolled_back;
pub use atomic::{atomic, atomic_immediate, on_commit, on_commit_pending, AtomicTx, TxGuard};

// `&Pool` dispatch. The `_pool` functions below take a [`Pool`],
// compile SQL through `pool.dialect()` and run it on the matching
// sqlx driver, so one call works against any backend. The older
// `&PgPool`-typed functions still work for existing callers.
use super::Pool;

/// MySQL counterpart of [`bind_query`].
#[cfg(feature = "mysql")]
pub(crate) fn bind_query_my(
    q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    value: SqlValue,
) -> sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    bind_match_mysql!(q, value)
}

/// SQLite counterpart of [`bind_query`].
#[cfg(feature = "sqlite")]
pub(crate) fn bind_query_sqlite<'a>(
    q: sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>>,
    value: SqlValue,
) -> sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>> {
    bind_match_sqlite!(q, value)
}

/// `INSERT` on any backend: compiled for the [`Pool`]'s dialect.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn insert_pool(pool: &Pool, query: &InsertQuery) -> Result<(), ExecError> {
    query.validate()?;
    let stmt = pool.dialect().compile_insert(query)?;
    execute_pool(pool, &stmt.sql, stmt.params).await?;
    Ok(())
}

/// `INSERT` that skips a duplicate: `Ok(true)` when the row went in,
/// `Ok(false)` when a unique key already held it. `query.on_conflict` is
/// replaced with `DoNothing`.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn insert_or_ignore(pool: &Pool, query: &InsertQuery) -> Result<bool, ExecError> {
    let query = InsertQuery {
        on_conflict: Some(crate::core::ConflictClause::DoNothing),
        ..query.clone()
    };
    query.validate()?;
    let stmt = pool.dialect().compile_insert(&query)?;
    match pool {
        // A skip reports one affected row here too (CLIENT_FOUND_ROWS),
        // so read the insert id the conflict clause sets instead.
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            crate::test_assertions::query_counter::bump();
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let done = q.execute(my).await?;
            Ok(done.rows_affected() > 0
                && done.last_insert_id() != crate::sql::mysql::SKIPPED_INSERT_ID)
        }
        #[allow(unreachable_patterns)]
        _ => Ok(execute_pool(pool, &stmt.sql, stmt.params).await? > 0),
    }
}

/// [`insert_or_ignore`] inside an open transaction.
///
/// # Errors
/// As [`insert_or_ignore`].
pub(crate) async fn insert_or_ignore_tx(
    tx: &mut PoolTx<'_>,
    query: &InsertQuery,
) -> Result<bool, ExecError> {
    let query = InsertQuery {
        on_conflict: Some(crate::core::ConflictClause::DoNothing),
        ..query.clone()
    };
    query.validate()?;
    let stmt = tx.dialect().compile_insert(&query)?;
    #[cfg(feature = "mysql")]
    #[allow(irrefutable_let_patterns)]
    {
        let gate = atomic::gate(tx)?;
        if let PoolTx::Mysql(t) = tx {
            // A skip reports one affected row too: read the insert id instead.
            crate::test_assertions::query_counter::bump();
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let done = gate.check(q.execute(&mut **t).await.map_err(ExecError::from))?;
            return Ok(done.rows_affected() > 0
                && done.last_insert_id() != crate::sql::mysql::SKIPPED_INSERT_ID);
        }
    }
    Ok(execute_tx(tx, &stmt.sql, stmt.params).await? > 0)
}

/// `INSERT` that returns the new row's data, on any backend. The
/// result shape differs per backend, so callers `match` on
/// [`InsertReturningPool`].
///
/// Postgres and SQLite use `INSERT … RETURNING` and give back the
/// whole row, with every requested column.
///
/// MySQL has no `RETURNING`, so it reads the insert id from the INSERT's
/// own reply. That is one auto-increment value, so `query.returning` must
/// name exactly one column, the model's `Auto<T>` PK. A skipped
/// `DoNothing` is `RowNotFound` on every backend.
///
/// # Errors
/// - [`ExecError::EmptyReturning`] when `query.returning` is empty.
/// - [`SqlError::OperatorNotSupportedInDialect`](crate::sql::SqlError::OperatorNotSupportedInDialect)
///   when MySQL is asked for a multi-column RETURNING.
/// - Validation, SQL-writing, or driver failures otherwise.
pub async fn insert_returning_pool(
    pool: &Pool,
    query: &InsertQuery,
) -> Result<InsertReturningPool, ExecError> {
    crate::test_assertions::query_counter::bump();
    query.validate()?;
    if query.returning.is_empty() {
        return Err(ExecError::EmptyReturning);
    }
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let row = insert_returning_on(pg, query).await?;
            Ok(InsertReturningPool::PgRow(row))
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => insert_mysql_auto_id(my, query).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            // SQLite 3.35+ has `INSERT … RETURNING`, so this mirrors
            // the Postgres path.
            let stmt = pool.dialect().compile_insert(query)?;
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let row = q.fetch_one(sq).await?;
            Ok(InsertReturningPool::SqliteRow(row))
        }
    }
}

/// What [`insert_returning_pool`] gives back: a full row on Postgres
/// and SQLite, or just the auto-assigned `i64` PK on MySQL.
pub enum InsertReturningPool {
    #[cfg(feature = "postgres")]
    PgRow(PgRow),
    #[cfg(feature = "mysql")]
    MySqlAutoId(i64),
    #[cfg(feature = "sqlite")]
    SqliteRow(sqlx::sqlite::SqliteRow),
}

impl ::core::fmt::Debug for InsertReturningPool {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
            #[cfg(feature = "postgres")]
            Self::PgRow(_) => f.debug_tuple("PgRow").field(&"<PgRow>").finish(),
            #[cfg(feature = "mysql")]
            Self::MySqlAutoId(id) => f.debug_tuple("MySqlAutoId").field(id).finish(),
            #[cfg(feature = "sqlite")]
            Self::SqliteRow(_) => f.debug_tuple("SqliteRow").field(&"<SqliteRow>").finish(),
        }
    }
}

/// Run `query` without RETURNING and take the id from its own OK packet.
/// `SELECT LAST_INSERT_ID()` would read session state an earlier skip left.
#[cfg(feature = "mysql")]
async fn insert_mysql_auto_id<'c, E>(
    exec: E,
    query: &InsertQuery,
) -> Result<InsertReturningPool, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
{
    // The OK packet only carries an AUTO_INCREMENT id: refuse before the
    // write, or the row lands and its PK is lost (#1978, #1986). Only the
    // PK: other RETURNING columns (e.g. `generated_as`) stay unread here.
    if let Some(pk) = query.model.primary_key().filter(|pk| {
        query.returning.contains(&pk.column)
            && !query.columns.contains(&pk.column)
            && !pk.is_serial()
    }) {
        return Err(ExecError::GeneratedPkUnreadable {
            table: query.model.table,
            column: pk.column,
        });
    }
    let stmt = super::mysql::DIALECT.compile_insert_auto_id(query)?;
    let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
        sqlx::query(&stmt.sql);
    for v in stmt.params {
        q = bind_query_my(q, v);
    }
    let done = q.execute(exec).await?;
    // A skipped `DoNothing` inserted no row, as RETURNING shows elsewhere.
    if done.last_insert_id() == crate::sql::mysql::SKIPPED_INSERT_ID {
        return Err(sqlx::Error::RowNotFound.into());
    }
    // `Auto<T>` is i64; ids above 2^63 cannot occur below the skip marker.
    let id = i64::try_from(done.last_insert_id()).unwrap_or(i64::MAX);
    Ok(InsertReturningPool::MySqlAutoId(id))
}

/// The PK of the row `q` just inserted: the submitted value when the PK
/// column is in the INSERT, else the database-generated one.
pub(crate) fn inserted_pk(
    q: &InsertQuery,
    returning: &crate::sql::InsertReturningPool,
    pk_field: &crate::core::FieldSchema,
) -> Result<SqlValue, ExecError> {
    match q.columns.iter().position(|c| *c == pk_field.column) {
        Some(i) => Ok(q.values[i].clone()),
        None => generated_pk(returning, pk_field, q.model.table),
    }
}

/// Read a generated PK out of an INSERT's RETURNING, or MySQL's
/// `LAST_INSERT_ID()`. A PK it can't read is an error, never a stand-in.
fn generated_pk(
    returning: &crate::sql::InsertReturningPool,
    pk_field: &crate::core::FieldSchema,
    table: &'static str,
) -> Result<SqlValue, ExecError> {
    use crate::core::FieldType as T;
    let unreadable = || ExecError::GeneratedPkUnreadable {
        table,
        column: pk_field.column,
    };
    #[allow(unused_variables)]
    {
        match returning {
            #[cfg(feature = "postgres")]
            crate::sql::InsertReturningPool::PgRow(row) => {
                use crate::sql::sqlx::Row as _;
                let c = pk_field.column;
                Ok(match pk_field.ty {
                    T::I64 => SqlValue::I64(row.try_get(c)?),
                    T::I32 => SqlValue::I32(row.try_get(c)?),
                    T::I16 => SqlValue::I16(row.try_get(c)?),
                    T::String => SqlValue::String(row.try_get(c)?),
                    T::Uuid => SqlValue::Uuid(row.try_get(c)?),
                    _ => return Err(unreadable()),
                })
            }
            #[cfg(feature = "mysql")]
            crate::sql::InsertReturningPool::MySqlAutoId(id) => match pk_field.ty {
                T::I64 => Ok(SqlValue::I64(*id)),
                T::I32 => Ok(SqlValue::I32(*id as i32)),
                T::I16 => Ok(SqlValue::I16(*id as i16)),
                _ => Err(unreadable()),
            },
            #[cfg(feature = "sqlite")]
            crate::sql::InsertReturningPool::SqliteRow(row) => {
                use crate::sql::sqlx::Row as _;
                let c = pk_field.column;
                Ok(match pk_field.ty {
                    T::I64 => SqlValue::I64(row.try_get(c)?),
                    T::I32 => SqlValue::I32(row.try_get(c)?),
                    T::I16 => SqlValue::I16(row.try_get(c)?),
                    T::String => SqlValue::String(row.try_get(c)?),
                    T::Uuid => sqlite_uuid_key(row, c)?,
                    _ => return Err(unreadable()),
                })
            }
        }
    }
}

/// Bound values are BLOBs; a column default may be TEXT. A TEXT
/// key stays text, so a re-read by it binds what is stored.
#[cfg(feature = "sqlite")]
fn sqlite_uuid_key(row: &sqlx::sqlite::SqliteRow, c: &str) -> Result<SqlValue, sqlx::Error> {
    use crate::sql::sqlx::Row as _;
    match row.try_get::<uuid::Uuid, _>(c) {
        Ok(u) => Ok(SqlValue::Uuid(u)),
        Err(e) => row
            .try_get::<String, _>(c)
            .ok()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
            .map(SqlValue::String)
            .ok_or(e),
    }
}

/// `field` of every row `query` selects, as each row stores it.
#[cfg(all(
    feature = "sqlite",
    any(feature = "admin", feature = "tenancy", feature = "template_views")
))]
async fn sqlite_stored_column(
    sq: &sqlx::SqlitePool,
    query: &SelectQuery,
    field: &'static crate::core::FieldSchema,
) -> Result<Vec<SqlValue>, ExecError> {
    let query = query.clone().projection(vec![field.column]);
    crate::test_assertions::query_counter::bump();
    let stmt = crate::sql::Dialect::compile_select(crate::sql::sqlite::DIALECT, &query)?;
    let mut q = sqlx::query(&stmt.sql);
    for v in stmt.params {
        q = bind_query_sqlite(q, v);
    }
    let rows = q.fetch_all(sq).await?;
    Ok(rows
        .iter()
        .map(|row| sqlite_uuid_key(row, field.column))
        .collect::<Result<_, _>>()?)
}

/// The UUID keys of the rows `query` selects, as stored; `None` where the
/// parsed form is the stored form (every backend but SQLite).
#[cfg(feature = "template_views")]
pub(crate) async fn select_stored_uuids(
    pool: &Pool,
    query: &SelectQuery,
    field: &'static crate::core::FieldSchema,
) -> Result<Option<Vec<SqlValue>>, ExecError> {
    #[cfg(feature = "sqlite")]
    if let (Pool::Sqlite(sq), crate::core::FieldType::Uuid) = (pool, field.ty) {
        return sqlite_stored_column(sq, query, field).await.map(Some);
    }
    let _ = (pool, query, field);
    Ok(None)
}

/// `key` (parsed from `raw`) as the row stores it. Only a SQLite UUID key
/// differs: BLOB when bound, TEXT when a column default wrote it.
/// `None` when the UUID is stored in more than one form, i.e. names several rows.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "template_views"))]
pub(crate) async fn stored_key(
    pool: &Pool,
    model: &'static crate::core::ModelSchema,
    pk_field: &'static crate::core::FieldSchema,
    key: SqlValue,
    raw: &str,
) -> Result<Option<SqlValue>, ExecError> {
    let mut keys = stored_keys(pool, model, pk_field, vec![(key, raw)]).await?;
    Ok(keys.remove(0))
}

/// [`stored_key`] for many `(key, raw)` pairs of `field`, in one read per chunk.
#[cfg(any(feature = "admin", feature = "tenancy", feature = "template_views"))]
pub(crate) async fn stored_keys(
    pool: &Pool,
    model: &'static crate::core::ModelSchema,
    field: &'static crate::core::FieldSchema,
    keys: Vec<(SqlValue, &str)>,
) -> Result<Vec<Option<SqlValue>>, ExecError> {
    #[cfg(feature = "sqlite")]
    if let (Pool::Sqlite(sq), crate::core::FieldType::Uuid) = (pool, field.ty) {
        let uuid_of = |v: &SqlValue| match v {
            SqlValue::Uuid(u) => Some(*u),
            SqlValue::String(s) => uuid::Uuid::parse_str(s).ok(),
            _ => None,
        };
        let mut stored: std::collections::HashMap<uuid::Uuid, Vec<SqlValue>> =
            std::collections::HashMap::new();
        // Four binds per key, well under SQLite's 32766.
        for chunk in keys.chunks(4096) {
            // The raw spelling, plus the canonical text forms a default may write.
            let binds = chunk
                .iter()
                .flat_map(|(k, raw)| {
                    let mut b = vec![k.clone(), SqlValue::String((*raw).to_owned())];
                    if let Some(u) = uuid_of(k) {
                        b.push(SqlValue::String(u.hyphenated().to_string()));
                        b.push(SqlValue::String(u.simple().to_string()));
                    }
                    b
                })
                .collect();
            let query = SelectQuery::by_pk_in(model, field.column, binds);
            for v in sqlite_stored_column(sq, &query, field).await? {
                if let Some(u) = uuid_of(&v) {
                    let forms = stored.entry(u).or_default();
                    if !forms.contains(&v) {
                        forms.push(v);
                    }
                }
            }
        }
        return Ok(keys
            .into_iter()
            .map(|(k, _)| match uuid_of(&k).and_then(|u| stored.get(&u)) {
                Some(forms) if forms.len() > 1 => None,
                Some(forms) => forms.first().cloned(),
                None => Some(k),
            })
            .collect());
    }
    let _ = (pool, model, field);
    Ok(keys.into_iter().map(|(k, _)| Some(k)).collect())
}

/// `UPDATE` against either backend; returns rows affected.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn update_pool(pool: &Pool, query: &UpdateQuery) -> Result<u64, ExecError> {
    query.validate()?;
    let stmt = pool.dialect().compile_update(query)?;
    execute_pool(pool, &stmt.sql, stmt.params).await
}

/// `DELETE` against either backend; returns rows affected.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn delete_pool(pool: &Pool, query: &DeleteQuery) -> Result<u64, ExecError> {
    let plan = crate::sql::compile_delete_plan(pool.dialect(), query)?;
    delete_plan_pool(pool, plan).await
}

/// Run `queries` all or none, for the generated `destroy` past the bind
/// cap: one transaction (a savepoint inside `atomic`) when more than one.
///
/// # Errors
/// As [`delete_pool`].
#[doc(hidden)]
pub async fn __rustango_delete_all_or_none(
    pool: &Pool,
    queries: Vec<DeleteQuery>,
) -> Result<u64, ExecError> {
    if let [query] = queries.as_slice() {
        return delete_pool(pool, query).await;
    }
    let plans = queries
        .iter()
        .map(|q| crate::sql::compile_delete_plan(pool.dialect(), q))
        .collect::<Result<Vec<_>, _>>()?;
    let run = atomic(pool, move |tx| {
        Box::pin(async move {
            let mut guard = tx.lock().await?;
            let mut deleted = 0;
            for plan in plans {
                deleted += run_delete_plan_tx(&mut guard, plan).await?;
            }
            Ok(deleted)
        })
    });
    crate::audit::cascade::via_pool(pool, run).await
}

/// Run a [`DeletePlan`](crate::sql::DeletePlan) on `pool`, in one
/// transaction (a savepoint inside `atomic`) when it has links.
pub(crate) async fn delete_plan_pool(
    pool: &Pool,
    plan: crate::sql::DeletePlan,
) -> Result<u64, ExecError> {
    if plan.is_one_statement() {
        return execute_pool(pool, &plan.owner.sql, plan.owner.params).await;
    }
    let run = atomic(pool, move |tx| {
        Box::pin(async move {
            let mut guard = tx.lock().await?;
            run_delete_plan_tx(&mut guard, plan).await
        })
    });
    crate::audit::cascade::via_pool(pool, run).await
}

/// Run a [`DeletePlan`](crate::sql::DeletePlan) in `tx`: links, then the
/// owner, auditing the audited rows it cascades to.
pub(crate) async fn run_delete_plan_tx(
    tx: &mut PoolTx<'_>,
    plan: crate::sql::DeletePlan,
) -> Result<u64, ExecError> {
    match plan.cascade {
        Some(query) => crate::audit::cascade::delete_with_audit(tx, &query).await,
        None => run_delete_statements_tx(tx, plan).await,
    }
}

/// A plan's statements in `tx`: links, then the owner; returns the owner's count.
pub(crate) async fn run_delete_statements_tx(
    tx: &mut PoolTx<'_>,
    plan: crate::sql::DeletePlan,
) -> Result<u64, ExecError> {
    for link in plan.links {
        execute_tx(tx, &link.sql, link.params).await?;
    }
    execute_tx(tx, &plan.owner.sql, plan.owner.params).await
}

/// `SELECT COUNT(*)` against either backend.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn count_rows_pool(pool: &Pool, query: &CountQuery) -> Result<i64, ExecError> {
    count_rows_on(pool, query).await
}

/// Multi-row `INSERT` on any backend. It does not read back `Auto<T>`
/// PKs the way [`bulk_insert_on`] does on Postgres, so the rows you
/// passed in keep their unset PKs.
///
/// Large batches are split to fit the backend's bind-parameter limit
/// and run in one transaction, so a failing batch rolls back the rest.
/// Inside an [`atomic`] block on the same pool, any size joins it as a
/// savepoint; do not hold its [`AtomicTx`] guard across this call.
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn bulk_insert_pool(pool: &Pool, query: &BulkInsertQuery) -> Result<(), ExecError> {
    if query.rows.is_empty() {
        return Ok(());
    }
    // Keep every `pool.dialect()` a temporary: it yields `&dyn Dialect`,
    // which is not `Sync`, so holding one across an `.await` would make
    // this future non-Send.
    let stmts = compile_bulk_insert_batches(pool.dialect(), query)?;
    execute_all_or_none(pool, stmts).await.map(drop)
}

/// Rows per batched statement: `binds_per_row` binds each under the
/// dialect's cap, and at most `max`.
pub(crate) fn rows_per_batch(dialect: &dyn Dialect, binds_per_row: usize, max: usize) -> usize {
    (dialect.max_bind_params() / binds_per_row.max(1)).clamp(1, max.max(1))
}

/// Run `stmts` all or none; returns rows affected. One statement alone
/// needs no transaction, but inside an [`atomic`] on `pool` any count
/// joins it as a savepoint.
pub(crate) async fn execute_all_or_none(
    pool: &Pool,
    mut stmts: Vec<super::CompiledStatement>,
) -> Result<u64, ExecError> {
    if stmts.len() == 1 && !atomic::in_block(pool) {
        let stmt = stmts.remove(0);
        return execute_pool(pool, &stmt.sql, stmt.params).await;
    }
    atomic(pool, move |tx| {
        Box::pin(async move {
            let mut guard = tx.lock().await?;
            execute_batches_tx(&mut guard, stmts).await
        })
    })
    .await
}

/// Run `stmts` in order in `tx`; returns the rows they affected.
pub(crate) async fn execute_batches_tx(
    tx: &mut PoolTx<'_>,
    stmts: Vec<super::CompiledStatement>,
) -> Result<u64, ExecError> {
    let mut affected = 0;
    for stmt in stmts {
        affected += execute_tx(tx, &stmt.sql, stmt.params).await?;
    }
    Ok(affected)
}

/// Check `query` against field limits like [`insert_pool`] does (#2153),
/// then compile it in batches that fit the backend's bind-parameter cap.
///
/// One multi-row INSERT binds `rows × columns` parameters: 65535 on
/// Postgres, 32766 on SQLite, `max_allowed_packet` on MySQL. Past the
/// cap the driver fails with an opaque error.
pub(crate) fn compile_bulk_insert_batches(
    dialect: &dyn Dialect,
    query: &BulkInsertQuery,
) -> Result<Vec<super::CompiledStatement>, ExecError> {
    query.validate()?;
    if query.rows.is_empty() {
        return Ok(Vec::new());
    }
    // Before chunking, so a repeated key resolves the same in any batch.
    let query = &*query.last_per_conflict_key();
    let max_rows = rows_per_batch(dialect, query.columns.len(), usize::MAX);
    if query.rows.len() <= max_rows {
        return Ok(vec![dialect.compile_bulk_insert(query)?]);
    }
    query
        .rows
        .chunks(max_rows)
        .map(|chunk| {
            dialect
                .compile_bulk_insert(&BulkInsertQuery {
                    model: query.model,
                    columns: query.columns.clone(),
                    rows: chunk.to_vec(),
                    returning: query.returning.clone(),
                    on_conflict: query.on_conflict.clone(),
                    generated: query.generated.clone(),
                })
                .map_err(ExecError::from)
        })
        .collect()
}

/// `UPDATE … FROM (VALUES …)` (Postgres) / `UPDATE … INNER JOIN
/// (VALUES …)` (MySQL); returns rows affected.
///
/// Rows past the bind cap go in batches inside one transaction (#2295).
///
/// # Errors
/// [`ExecError`] if the query is invalid or the driver rejects it.
pub async fn bulk_update_pool(pool: &Pool, query: &BulkUpdateQuery) -> Result<u64, ExecError> {
    if query.rows.is_empty() {
        return Ok(0);
    }
    let stmts = compile_bulk_update_batches(pool.dialect(), query, usize::MAX)?;
    execute_all_or_none(pool, stmts).await
}

/// Compile `query` in batches of at most `max` rows that fit the bind cap.
pub(crate) fn compile_bulk_update_batches(
    dialect: &dyn Dialect,
    query: &BulkUpdateQuery,
    max: usize,
) -> Result<Vec<super::CompiledStatement>, ExecError> {
    // Each row binds its PK plus one value per column (#2295).
    let max_rows = rows_per_batch(dialect, query.update_columns.len() + 1, max);
    query
        .rows
        .chunks(max_rows)
        .map(|chunk| {
            let batch =
                BulkUpdateQuery::new(query.model, query.update_columns.clone(), chunk.to_vec());
            dialect.compile_bulk_update(&batch).map_err(ExecError::from)
        })
        .collect()
}

/// Run arbitrary SQL with bound `SqlValue` params; returns rows
/// affected.
///
/// The SQL must use the dialect's placeholder shape. Build it with
/// `pool.dialect().placeholder(n)`. Note that `n` is ignored outside
/// Postgres, so binds must appear in the same order as their
/// placeholders in the text.
///
/// # Errors
/// Driver / SQL failures.
pub async fn raw_execute_pool(
    pool: &Pool,
    sql: &str,
    binds: Vec<SqlValue>,
) -> Result<u64, ExecError> {
    crate::sql::refuse_unbindable_values(pool.dialect(), &binds)?;
    execute_pool(pool, sql, binds).await
}

/// [`raw_execute_pool`] inside an open [`PoolTx`], so several writes
/// share one transaction. Finish with [`PoolTx::commit`].
///
/// # Errors
/// Driver / SQL failures.
pub async fn raw_execute_tx(
    tx: &mut tx::PoolTx<'_>,
    sql: &str,
    binds: Vec<SqlValue>,
) -> Result<u64, ExecError> {
    crate::sql::refuse_unbindable_values(tx.dialect(), &binds)?;
    let gate = atomic::gate(tx)?;
    gate.check(
        async {
            // Counts toward `assert_num_queries`; a no-op outside tests.
            crate::test_assertions::query_counter::bump();
            match tx {
                #[cfg(feature = "postgres")]
                tx::PoolTx::Postgres(t) => {
                    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
                    for v in binds {
                        q = bind_query(q, v);
                    }
                    Ok(q.execute(&mut **t).await?.rows_affected())
                }
                #[cfg(feature = "mysql")]
                tx::PoolTx::Mysql(t) => {
                    let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                        sqlx::query(sql);
                    for v in binds {
                        q = bind_query_my(q, v);
                    }
                    Ok(q.execute(&mut **t).await?.rows_affected())
                }
                #[cfg(feature = "sqlite")]
                tx::PoolTx::Sqlite(t) => {
                    let mut q: sqlx::query::Query<
                        '_,
                        sqlx::Sqlite,
                        sqlx::sqlite::SqliteArguments<'_>,
                    > = sqlx::query(sql);
                    for v in binds {
                        q = bind_query_sqlite(q, v);
                    }
                    Ok(q.execute(&mut **t).await?.rows_affected())
                }
            }
        }
        .await,
    )
}

/// Run a `;`-separated DDL script on any backend, safe to re-run.
/// Use it for the "make sure this table exists" step in modules that
/// ship their own schema constants. Empty fragments are skipped.
///
/// Errors that mean "the object already exists" are ignored, so a
/// second run succeeds. Everything else is returned.
///
/// # Errors
/// The driver's [`sqlx::Error`], except for the already-exists cases.
/// Non-driver `ExecError` variants map to `sqlx::Error::Protocol`.
pub async fn run_ddl_idempotent(pool: &Pool, ddl: &str) -> Result<(), sqlx::Error> {
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        match execute_pool(pool, stmt, Vec::new()).await {
            Ok(_) => {}
            Err(crate::sql::ExecError::Driver(err)) => {
                // Two ways a re-run can fail with the job already done:
                //
                //  * MySQL has no `CREATE INDEX IF NOT EXISTS`, so the
                //    second run raises ER_DUP_KEYNAME.
                //  * Postgres has that syntax but it is not atomic, so
                //    two processes starting together can both pass the
                //    existence check and one loses the race.
                //
                // The object exists either way, so continue.
                if !crate::sql::is_mysql_dup_index_error(&err)
                    && !crate::sql::is_pg_dup_object_error(&err)
                {
                    return Err(err);
                }
            }
            Err(other) => return Err(sqlx::Error::Protocol(format!("{other}"))),
        }
    }
    Ok(())
}

/// Run bind-free SQL unprepared, so a script of several statements runs on
/// every backend (PG and MySQL refuse that as one prepared statement).
///
/// # Errors
/// The driver's [`sqlx::Error`].
pub(crate) async fn execute_unprepared_pool(pool: &Pool, sql: &str) -> Result<(), sqlx::Error> {
    use sqlx::Executor as _;
    crate::test_assertions::query_counter::bump();
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => p.execute(sqlx::raw_sql(sql)).await.map(drop),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => p.execute(sqlx::raw_sql(sql)).await.map(drop),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => p.execute(sqlx::raw_sql(sql)).await.map(drop),
    }
}

// ---- internal dispatch helpers ----

/// Run a parameterized statement that returns no rows. Shared by the
/// non-`FromRow` `_pool` functions.
async fn execute_pool(pool: &Pool, sql: &str, binds: Vec<SqlValue>) -> Result<u64, ExecError> {
    // Counts toward `assert_num_queries`; a no-op outside tests.
    crate::test_assertions::query_counter::bump();
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
            for v in binds {
                q = bind_query(q, v);
            }
            Ok(q.execute(pg).await?.rows_affected())
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_my(q, v);
            }
            Ok(q.execute(my).await?.rows_affected())
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_sqlite(q, v);
            }
            Ok(q.execute(sq).await?.rows_affected())
        }
    }
}

// `&mut PoolTx` dispatch: the `_tx` helpers below mirror the `_pool`
// ones but run against an open transaction from `transaction_pool`.

// `insert_tx` / `update_tx` / `delete_tx` all funnel through here, so
// one query-counter bump covers all three. `raw_execute_tx` bumps on
// its own and does not reach this.
async fn execute_tx(
    tx: &mut PoolTx<'_>,
    sql: &str,
    binds: Vec<SqlValue>,
) -> Result<u64, ExecError> {
    let gate = atomic::gate(tx)?;
    gate.check(
        async {
            crate::test_assertions::query_counter::bump();
            match tx {
                #[cfg(feature = "postgres")]
                PoolTx::Postgres(t) => {
                    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
                    for v in binds {
                        q = bind_query(q, v);
                    }
                    Ok(q.execute(&mut **t).await?.rows_affected())
                }
                #[cfg(feature = "mysql")]
                PoolTx::Mysql(t) => {
                    let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                        sqlx::query(sql);
                    for v in binds {
                        q = bind_query_my(q, v);
                    }
                    Ok(q.execute(&mut **t).await?.rows_affected())
                }
                #[cfg(feature = "sqlite")]
                PoolTx::Sqlite(t) => {
                    let mut q: sqlx::query::Query<
                        '_,
                        sqlx::Sqlite,
                        sqlx::sqlite::SqliteArguments<'_>,
                    > = sqlx::query(sql);
                    for v in binds {
                        q = bind_query_sqlite(q, v);
                    }
                    Ok(q.execute(&mut **t).await?.rows_affected())
                }
            }
        }
        .await,
    )
}

/// [`insert_pool`] inside an open transaction, so the write joins the
/// caller's transaction.
///
/// # Errors
/// As [`insert_pool`].
pub async fn insert_tx(tx: &mut PoolTx<'_>, query: &InsertQuery) -> Result<(), ExecError> {
    query.validate()?;
    let stmt = tx.dialect().compile_insert(query)?;
    execute_tx(tx, &stmt.sql, stmt.params).await?;
    Ok(())
}

/// [`insert_returning_pool`] inside an open transaction.
///
/// # Errors
/// As [`insert_returning_pool`].
pub async fn insert_returning_tx(
    tx: &mut PoolTx<'_>,
    query: &InsertQuery,
) -> Result<InsertReturningPool, ExecError> {
    let gate = atomic::gate(tx)?;
    gate.check(
        async {
            crate::test_assertions::query_counter::bump();
            query.validate()?;
            if query.returning.is_empty() {
                return Err(ExecError::EmptyReturning);
            }
            match tx {
                #[cfg(feature = "postgres")]
                PoolTx::Postgres(t) => {
                    let row = insert_returning_on(&mut **t, query).await?;
                    Ok(InsertReturningPool::PgRow(row))
                }
                #[cfg(feature = "mysql")]
                PoolTx::Mysql(t) => insert_mysql_auto_id(&mut **t, query).await,
                #[cfg(feature = "sqlite")]
                PoolTx::Sqlite(t) => {
                    let stmt = super::sqlite::DIALECT.compile_insert(query)?;
                    let mut q: sqlx::query::Query<
                        '_,
                        sqlx::Sqlite,
                        sqlx::sqlite::SqliteArguments<'_>,
                    > = sqlx::query(&stmt.sql);
                    for v in stmt.params {
                        q = bind_query_sqlite(q, v);
                    }
                    let row = q.fetch_one(&mut **t).await?;
                    Ok(InsertReturningPool::SqliteRow(row))
                }
            }
        }
        .await,
    )
}

/// Run `query` in `tx` with `RETURNING <pk>`, in bind-sized batches, and
/// give back the PK of every row it wrote. PG and SQLite only.
///
/// # Errors
/// As [`bulk_insert_pool`]; [`ExecError::AuditUnsupported`] on MySQL.
pub(crate) async fn bulk_insert_pks_tx(
    tx: &mut PoolTx<'_>,
    query: &BulkInsertQuery,
) -> Result<Vec<SqlValue>, ExecError> {
    let pk = query
        .model
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: query.model.table,
        })?;
    #[cfg(feature = "mysql")]
    let no_returning = || ExecError::AuditUnsupported {
        table: query.model.table,
        reason: "MySQL has no RETURNING to read the written rows",
    };
    // Before compiling: the MySQL writer refuses RETURNING itself.
    #[cfg(feature = "mysql")]
    if matches!(tx, PoolTx::Mysql(_)) && !query.rows.is_empty() {
        return Err(no_returning());
    }
    let mut batch = BulkInsertQuery::new(query.model, query.columns.clone(), query.rows.clone())
        .returning(vec![pk.column]);
    batch.on_conflict.clone_from(&query.on_conflict);
    let stmts = compile_bulk_insert_batches(tx.dialect(), &batch)?;
    let mut pks = Vec::with_capacity(query.rows.len());
    for stmt in stmts {
        crate::test_assertions::query_counter::bump();
        let rows: Result<Vec<InsertReturningPool>, ExecError> = match tx {
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(t) => Ok(stmt
                .params
                .into_iter()
                .fold(sqlx::query(&stmt.sql), bind_query)
                .fetch_all(&mut **t)
                .await?
                .into_iter()
                .map(InsertReturningPool::PgRow)
                .collect()),
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(_) => {
                drop(stmt);
                Err(no_returning())
            }
            #[cfg(feature = "sqlite")]
            PoolTx::Sqlite(t) => Ok(stmt
                .params
                .into_iter()
                .fold(sqlx::query(&stmt.sql), bind_query_sqlite)
                .fetch_all(&mut **t)
                .await?
                .into_iter()
                .map(InsertReturningPool::SqliteRow)
                .collect()),
        };
        for row in &rows? {
            pks.push(generated_pk(row, pk, query.model.table)?);
        }
    }
    Ok(pks)
}

/// `UPDATE` inside an open transaction; returns rows affected.
///
/// # Errors
/// As [`update_pool`].
pub async fn update_tx(tx: &mut PoolTx<'_>, query: &UpdateQuery) -> Result<u64, ExecError> {
    query.validate()?;
    let stmt = tx.dialect().compile_update(query)?;
    execute_tx(tx, &stmt.sql, stmt.params).await
}

/// `DELETE` inside an open transaction; returns rows affected.
///
/// # Errors
/// As [`delete_pool`].
pub async fn delete_tx(tx: &mut PoolTx<'_>, query: &DeleteQuery) -> Result<u64, ExecError> {
    let plan = crate::sql::compile_delete_plan(tx.dialect(), query)?;
    run_delete_plan_tx(tx, plan).await
}

/// How far a truncate reaches past its own table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TruncateMode {
    /// Delete every row; each FK's `ON DELETE` applies, so a row that
    /// restricts the delete refuses the truncate.
    Plain,
    /// Also empty every table that references it, transitively, whatever
    /// its `ON DELETE`.
    Cascade,
}

/// The tables with an FK to `table`, transitively, from the catalog.
async fn referencing_tables(tx: &mut PoolTx<'_>, table: &str) -> Result<Vec<String>, ExecError> {
    let Some(sql) = tx.dialect().referencing_tables_sql() else {
        return Ok(Vec::new());
    };
    let mut seen = vec![table.to_owned()];
    let mut frontier = vec![table.to_owned()];
    while let Some(parent) = frontier.pop() {
        let rows: Vec<(String,)> = raw_query_tx(tx, sql, vec![SqlValue::String(parent)]).await?;
        for (child,) in rows {
            if !seen.iter().any(|t| t.eq_ignore_ascii_case(&child)) {
                seen.push(child.clone());
                frontier.push(child);
            }
        }
    }
    seen.remove(0);
    Ok(seen)
}

/// An FK refusal. SQLite reports a `RESTRICT` one as a trigger failure (1811).
fn is_fk_refusal(db: &dyn sqlx::error::DatabaseError) -> bool {
    db.is_foreign_key_violation()
        || (db.code().as_deref() == Some("1811") && db.message() == "FOREIGN KEY constraint failed")
}

/// A plain truncate's FK refusal, named for the table.
pub(crate) fn truncate_refusal(table: &'static str, mode: TruncateMode, e: ExecError) -> ExecError {
    match e {
        ExecError::Driver(sqlx::Error::Database(db))
            if mode == TruncateMode::Plain && is_fk_refusal(db.as_ref()) =>
        {
            ExecError::TruncateReferenced {
                table,
                source: sqlx::Error::Database(db),
            }
        }
        other => other,
    }
}

/// Truncate `table` in `tx` after clearing its generic M2M junction rows,
/// so a reused id inherits no links. The audited tables it takes rows from
/// through FKs get a bulk entry. Returns a plain truncate's deleted rows.
pub(crate) async fn truncate_tx(
    tx: &mut PoolTx<'_>,
    table: &'static str,
    mode: TruncateMode,
) -> Result<u64, ExecError> {
    for link in crate::sql::m2m::generic_m2m_truncate_links(table) {
        let stmt = tx.dialect().compile_delete(&link)?;
        execute_tx(tx, &stmt.sql, stmt.params).await?;
    }
    let referencing = match mode {
        TruncateMode::Plain => Vec::new(),
        TruncateMode::Cascade => referencing_tables(tx, table).await?,
    };
    let before = crate::audit::cascade::truncate_counts(tx, table, mode).await?;
    let mut affected = 0;
    for sql in tx.dialect().truncate_sql(table, mode, &referencing) {
        affected = execute_tx(tx, &sql, Vec::new())
            .await
            .map_err(|e| truncate_refusal(table, mode, e))?;
    }
    crate::audit::cascade::emit_truncated(tx, table, before).await?;
    // A plain truncate is one `DELETE`; `TRUNCATE` reports no count.
    Ok(if mode == TruncateMode::Plain {
        affected
    } else {
        0
    })
}

/// `DELETE` every row of `model`'s table, and its generic M2M junction
/// rows, in one transaction. Backs the derived `Model::truncate`.
///
/// Each FK's `ON DELETE` applies on every backend: a `RESTRICT` / `NO ACTION`
/// row still pointing here refuses the whole truncate. Ids keep counting.
///
/// # Errors
/// [`ExecError::TruncateReferenced`] while such a row exists; driver errors.
pub async fn truncate(
    pool: &Pool,
    model: &'static crate::core::ModelSchema,
) -> Result<u64, ExecError> {
    run_truncate(pool, model.table, TruncateMode::Plain).await
}

/// [`truncate`], also emptying every table that references `model`'s,
/// transitively and whatever its `ON DELETE`. `TRUNCATE … CASCADE` on
/// Postgres; on MySQL / SQLite a `DELETE` per table, FK checks off / deferred.
///
/// # Errors
/// Driver errors; nothing is emptied then.
pub async fn truncate_cascade(
    pool: &Pool,
    model: &'static crate::core::ModelSchema,
) -> Result<(), ExecError> {
    run_truncate(pool, model.table, TruncateMode::Cascade)
        .await
        .map(drop)
}

async fn run_truncate(
    pool: &Pool,
    table: &'static str,
    mode: TruncateMode,
) -> Result<u64, ExecError> {
    crate::audit::cascade::via_pool(pool, async {
        let mut tx = tx::write_transaction(pool).await?;
        let affected = truncate_tx(&mut tx, table, mode).await?;
        tx.commit()
            .await
            .map_err(|e| truncate_refusal(table, mode, e.into()))?;
        Ok(affected)
    })
    .await
}

/// [`bulk_insert_pool`] inside an open transaction, split to fit the
/// backend's bind-parameter limit the same way.
///
/// # Errors
/// As [`bulk_insert_pool`].
pub async fn bulk_insert_tx(tx: &mut PoolTx<'_>, query: &BulkInsertQuery) -> Result<(), ExecError> {
    let stmts = compile_bulk_insert_batches(tx.dialect(), query)?;
    execute_batches_tx(tx, stmts).await.map(drop)
}

/// `SELECT` inside an open transaction, with optional `select_related`
/// join decoding. Mirrors [`select_rows_pool_with_related`] but
/// executes against `tx`.
///
/// # Errors
/// As [`select_rows_pool_with_related`].
pub async fn select_rows_tx_with_related<T>(
    tx: &mut PoolTx<'_>,
    query: &SelectQuery,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    select_rows_with_related_on(tx, query).await
}

/// [`count_rows_pool`] inside an open transaction.
///
/// # Errors
/// As [`count_rows_pool`].
pub async fn count_tx(tx: &mut PoolTx<'_>, query: &CountQuery) -> Result<i64, ExecError> {
    count_rows_on(tx, query).await
}

/// [`count_rows_pool`] on any [`SqlExecutor`].
pub(crate) async fn count_rows_on<'c, E: SqlExecutor<'c>>(
    exec: E,
    query: &CountQuery,
) -> Result<i64, ExecError> {
    let stmt = exec.dialect().compile_count(query)?;
    count_stmt_on(exec, stmt).await
}

/// Run a compiled `SELECT COUNT(*)` on `exec`.
pub(crate) async fn count_stmt_on<'c, E: SqlExecutor<'c>>(
    exec: E,
    stmt: crate::sql::CompiledStatement,
) -> Result<i64, ExecError> {
    fn cell_i64<R>(row: &R) -> Result<i64, sqlx::Error>
    where
        R: sqlx::Row,
        usize: sqlx::ColumnIndex<R>,
        i64: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    {
        row.try_get(0)
    }
    let counts = run_on(exec, &stmt.sql, stmt.params, |row| match row {
        #[cfg(feature = "postgres")]
        RowRef::Postgres(r) => cell_i64(r),
        #[cfg(feature = "mysql")]
        RowRef::Mysql(r) => cell_i64(r),
        #[cfg(feature = "sqlite")]
        RowRef::Sqlite(r) => cell_i64(r),
    })
    .await?;
    Ok(counts.first().copied().ok_or(sqlx::Error::RowNotFound)?)
}

/// Run one statement on `exec`, decoding row by row, refused on an
/// `atomic` transaction the server already ended. Counted here, so every
/// `_on` read is.
pub(crate) async fn run_on<'c, E, U, F>(
    exec: E,
    sql: &str,
    params: Vec<SqlValue>,
    decode: F,
) -> Result<Vec<U>, ExecError>
where
    E: SqlExecutor<'c>,
    U: Send,
    F: FnMut(RowRef<'_>) -> Result<U, sqlx::Error> + Send,
{
    let guard = exec.guard()?;
    crate::test_assertions::query_counter::bump();
    guard.check(
        exec.fetch_rows(sql, params, decode)
            .await
            .map_err(ExecError::from),
    )
}

/// [`run_on`] for a statement that returns no rows: the rows affected.
pub(crate) async fn write_on<'c, E: SqlExecutor<'c>>(
    exec: E,
    sql: &str,
    params: Vec<SqlValue>,
) -> Result<u64, ExecError> {
    let guard = exec.guard()?;
    crate::test_assertions::query_counter::bump();
    guard.check(
        exec.execute_rows(sql, params)
            .await
            .map_err(ExecError::from),
    )
}

mod traits;
#[cfg(feature = "mysql")]
pub use traits::LoadRelatedMy;
#[cfg(feature = "sqlite")]
pub use traits::LoadRelatedSqlite;
pub use traits::{
    MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow, MaybeSqliteFromRow, MaybeSqliteLoadRelated,
};

/// Run a `SelectQuery` on any backend and decode each row into `T`.
/// It ignores `select_related` joins; use
/// [`select_rows_pool_with_related`] when the query has them.
///
/// # Errors
/// [`ExecError`] if the query is invalid, the driver rejects it, or a
/// column does not decode into `T`.
pub async fn select_rows_pool<T>(pool: &Pool, query: &SelectQuery) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
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
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_all(sq).await?)
        }
    }
}

/// Single-row variant of [`select_rows_pool`]. Returns `Ok(None)`
/// when no rows match.
///
/// # Errors
/// [`ExecError`] if the query is invalid, the driver rejects it, or a
/// column does not decode into `T`.
pub async fn select_one_row_pool<T>(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Option<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_optional(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_my(q, v);
            }
            Ok(q.fetch_optional(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryAs<
                '_,
                sqlx::Sqlite,
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_optional(sq).await?)
        }
    }
}

/// MySQL-typed `QueryAs` binding helper, symmetric with [`bind_query_as`].
#[cfg(feature = "mysql")]
pub(super) fn bind_query_as_my<T>(
    q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments>,
    value: SqlValue,
) -> sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> {
    bind_match_mysql!(q, value)
}

/// SQLite-typed `QueryAs` binding helper, symmetric with [`bind_query_as`].
#[cfg(feature = "sqlite")]
pub(super) fn bind_query_as_sqlite<'a, T>(
    q: sqlx::query::QueryAs<'a, sqlx::Sqlite, T, sqlx::sqlite::SqliteArguments<'a>>,
    value: SqlValue,
) -> sqlx::query::QueryAs<'a, sqlx::Sqlite, T, sqlx::sqlite::SqliteArguments<'a>> {
    bind_match_sqlite!(q, value)
}

/// Aggregates on any backend: runs an `AggregateQuery`
/// (GROUP BY / HAVING / aggregate expressions) and decodes each row
/// into `T`.
///
/// # Errors
/// [`ExecError`] if the query is invalid, the driver rejects it, or a
/// column does not decode into `T`.
pub async fn fetch_aggregate_pool<T>(
    pool: &Pool,
    query: &AggregateQuery,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_aggregate(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
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
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_all(sq).await?)
        }
    }
}

// `.values_dict()` / `.values_list()` projection.
mod values;
#[allow(unused_imports)]
pub use values::{
    fetch_aggregate_dict, fetch_values_dict, fetch_values_flat, fetch_values_list, try_get_flat,
    try_get_flat_my, try_get_flat_sqlite, FlatScalar, MaybeMyScalar, MaybePgScalar,
    MaybeSqliteScalar,
};
pub(crate) use values::{fetch_values_flat_on, fetch_values_list_on};

/// Raw SQL on any backend: runs it with bound `SqlValue`
/// params and decodes each row into `T`.
///
/// The SQL must use the dialect's placeholder shape. Build it with
/// `pool.dialect().placeholder(n)`. Note that `n` is ignored outside
/// Postgres, so binds must appear in the same order as their
/// placeholders in the text.
///
/// # Errors
/// [`ExecError`] if the driver rejects the SQL, or a column does not
/// decode into `T`.
pub async fn raw_query_pool<T>(
    sql: &str,
    binds: Vec<SqlValue>,
    pool: &Pool,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    crate::sql::refuse_unbindable_values(pool.dialect(), &binds)?;
    crate::test_assertions::query_counter::bump();
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> = sqlx::query_as::<_, T>(sql);
            for v in binds {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(sql);
            for v in binds {
                q = bind_query_as_my(q, v);
            }
            Ok(q.fetch_all(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryAs<
                '_,
                sqlx::Sqlite,
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(sql);
            for v in binds {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_all(sq).await?)
        }
    }
}

/// [`raw_query_pool`] inside an open [`PoolTx`], for reads that must
/// see the transaction: read-after-write, `FOR UPDATE` locks, and
/// lookup-then-modify flows.
///
/// # Errors
/// Driver / SQL failures.
pub async fn raw_query_tx<T>(
    tx: &mut tx::PoolTx<'_>,
    sql: &str,
    binds: Vec<SqlValue>,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    crate::sql::refuse_unbindable_values(tx.dialect(), &binds)?;
    let gate = atomic::gate(tx)?;
    gate.check(
        async {
            // Counts toward `assert_num_queries`; a no-op outside tests.
            crate::test_assertions::query_counter::bump();
            match tx {
                #[cfg(feature = "postgres")]
                tx::PoolTx::Postgres(t) => {
                    let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                        sqlx::query_as::<_, T>(sql);
                    for v in binds {
                        q = bind_query_as(q, v);
                    }
                    Ok(q.fetch_all(&mut **t).await?)
                }
                #[cfg(feature = "mysql")]
                tx::PoolTx::Mysql(t) => {
                    let mut q: sqlx::query::QueryAs<
                        '_,
                        sqlx::MySql,
                        T,
                        sqlx::mysql::MySqlArguments,
                    > = sqlx::query_as::<_, T>(sql);
                    for v in binds {
                        q = bind_query_as_my(q, v);
                    }
                    Ok(q.fetch_all(&mut **t).await?)
                }
                #[cfg(feature = "sqlite")]
                tx::PoolTx::Sqlite(t) => {
                    let mut q: sqlx::query::QueryAs<
                        '_,
                        sqlx::Sqlite,
                        T,
                        sqlx::sqlite::SqliteArguments<'_>,
                    > = sqlx::query_as::<_, T>(sql);
                    for v in binds {
                        q = bind_query_as_sqlite(q, v);
                    }
                    Ok(q.fetch_all(&mut **t).await?)
                }
            }
        }
        .await,
    )
}

/// Run a `.dates(field, kind)` queryset. Wraps its SELECT in
/// `SELECT DISTINCT <trunc(col)> FROM (<inner>) ORDER BY …` to get
/// the distinct truncated dates.
///
/// # Errors
/// - [`ExecError::Query`] forwarded from the underlying
///   [`raw_query_pool`] call.
/// - Driver / SQL errors on decode (e.g. NULL date columns surface as
///   sqlx decode errors when the column type isn't `Option<NaiveDate>`).
pub async fn fetch_dates_pool<T: crate::core::Model + Send>(
    pool: &Pool,
    qs: crate::query::DatesQuerySet<T>,
) -> Result<Vec<chrono::NaiveDate>, ExecError> {
    let column = qs.resolve_column()?;
    // The inner SELECT keeps WHERE / JOINs / LIMIT; the outer orders by bucket.
    let bucket = crate::sql::DateBucket::Date(qs.kind);
    let descending = qs.descending;
    let stmt = crate::sql::compile_date_buckets(
        pool.dialect(),
        &qs.qs.compile()?,
        column,
        bucket,
        descending,
    )?;
    let rows: Vec<(chrono::NaiveDate,)> = raw_query_pool(&stmt.sql, stmt.params, pool).await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// Run a `.datetimes(field, kind)` queryset: [`fetch_dates_pool`] with
/// finer buckets (`Hour` / `Minute` / `Second`) and a `DateTime<Utc>`
/// result.
///
/// # Errors
/// Same shape as [`fetch_dates_pool`].
pub async fn fetch_datetimes_pool<T: crate::core::Model + Send>(
    pool: &Pool,
    qs: crate::query::DateTimesQuerySet<T>,
) -> Result<Vec<chrono::DateTime<chrono::Utc>>, ExecError> {
    let column = qs.resolve_column()?;
    let bucket = crate::sql::DateBucket::DateTime(qs.kind);
    let descending = qs.descending;
    let stmt = crate::sql::compile_date_buckets(
        pool.dialect(),
        &qs.qs.compile()?,
        column,
        bucket,
        descending,
    )?;
    let rows: Vec<(chrono::DateTime<chrono::Utc>,)> =
        raw_query_pool(&stmt.sql, stmt.params, pool).await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// Gives `QuerySet` a `count(&Pool)` method that works on any backend.
///
/// Import with `use rustango::sql::CounterPool;`.
pub trait CounterPool<T: Model + Send> {
    /// Count rows matching the queryset's filters.
    ///
    /// # Errors
    /// [`ExecError`] for schema, SQL-writing, or driver failures.
    fn count(self, pool: &Pool)
        -> impl std::future::Future<Output = Result<i64, ExecError>> + Send;
}

impl<T: Model + Send> CounterPool<T> for QuerySet<T> {
    async fn count(self, pool: &Pool) -> Result<i64, ExecError> {
        let Some(select) = self.compile_unless_none()? else {
            return Ok(0);
        };
        count_rows_pool(pool, &CountQuery::from_select(select)).await
    }
}

/// Boolean predicates on a `QuerySet`: `exists`,
/// `is_empty`, `doesnt_exist` and `contains_pk`.
///
/// All of them count at most one row (`LIMIT 1` in a derived table), so
/// they stop at the first match.
///
/// Import with `use rustango::sql::ExistsPool;`.
pub trait ExistsPool<T: Model + Send> {
    /// `Ok(true)` when at least one row matches the queryset's
    /// filters.
    ///
    /// # Errors
    /// As [`CounterPool::count`].
    fn exists(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<bool, ExecError>> + Send;

    /// `Ok(true)` when the queryset matches no rows. The inverse of
    /// [`Self::exists`], easier to read than `!qs.exists(..).await?`.
    ///
    /// # Errors
    /// As [`Self::exists`].
    fn is_empty(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<bool, ExecError>> + Send;

    /// Alias for [`Self::is_empty`], named after Eloquent's
    /// `doesntExist()`.
    ///
    /// # Errors
    /// As [`Self::is_empty`].
    fn doesnt_exist(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<bool, ExecError>> + Send;

    /// `Ok(true)` when the queryset contains the row with this PK.
    /// The PK column comes from `T::SCHEMA`.
    ///
    /// # Errors
    /// As [`Self::exists`], plus an `ExecError::Query` when the model
    /// has no primary key.
    fn contains_pk(
        self,
        pool: &Pool,
        pk_value: impl Into<crate::core::SqlValue> + Send,
    ) -> impl std::future::Future<Output = Result<bool, ExecError>> + Send;
}

impl<T: Model + Send> ExistsPool<T> for QuerySet<T> {
    async fn exists(self, pool: &Pool) -> Result<bool, ExecError> {
        let Some(select) = self.compile_unless_none()? else {
            return Ok(false);
        };
        Ok(count_rows_pool(pool, &CountQuery::exists(select)).await? > 0)
    }

    async fn is_empty(self, pool: &Pool) -> Result<bool, ExecError> {
        Ok(!self.exists(pool).await?)
    }

    async fn doesnt_exist(self, pool: &Pool) -> Result<bool, ExecError> {
        self.is_empty(pool).await
    }

    async fn contains_pk(
        self,
        pool: &Pool,
        pk_value: impl Into<crate::core::SqlValue> + Send,
    ) -> Result<bool, ExecError> {
        let Some(pk_field) = T::SCHEMA.primary_key() else {
            return Err(ExecError::Query(crate::core::QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: "primary_key".into(),
            }));
        };
        self.filter_op(pk_field.name, crate::core::Op::Eq, pk_value.into())
            .exists(pool)
            .await
    }
}

/// [`QuerySet::fetch_paginated_on`] on any backend: a page of rows
/// and the pre-LIMIT total in one round trip, via `COUNT(*) OVER ()`.
/// An empty result gives `Page { rows: vec![], total: 0 }`.
///
/// MySQL needs 8.0 or later for that window function. On MySQL 5.7,
/// call `count_rows_pool` and `select_rows_pool` separately.
///
/// # Errors
/// As [`QuerySet::fetch_paginated_on`].
pub async fn fetch_paginated_pool<T>(
    qs: crate::query::QuerySet<T>,
    pool: &Pool,
) -> Result<Page<T>, ExecError>
where
    T: Model + MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    qs.fetch_paginated_on(pool).await
}

mod prefetch;
pub use prefetch::{
    __reverse_fk_key, fetch_with_prefetch_filtered, fetch_with_prefetch_pool,
    fetch_with_prefetch_related, fetch_with_prefetch_related_filtered,
};

/// [`select_rows_pool`] that also decodes `select_related` joins.
/// With no joins it takes the same fast path. With joins it fetches
/// raw rows and stitches each join alias onto the decoded model.
///
/// The extra [`LoadRelated`] and [`MaybeMyLoadRelated`] bounds are
/// satisfied by every `#[derive(Model)]` type.
///
/// # Errors
/// As [`select_rows_pool`].
pub async fn select_rows_pool_with_related<T>(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    select_rows_with_related_on(pool, query).await
}

/// [`select_rows_pool_with_related`] on any [`SqlExecutor`].
pub(crate) async fn select_rows_with_related_on<'c, T, E>(
    exec: E,
    query: &SelectQuery,
) -> Result<Vec<T>, ExecError>
where
    E: SqlExecutor<'c>,
    T: MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    select_rows_mapped_on(exec, query, None, |_, t| t).await
}

/// [`select_rows_with_related_on`], each row paired with its `key` result
/// column decoded as the given type.
pub(crate) async fn select_rows_keyed_on<'c, T, E>(
    exec: E,
    query: &SelectQuery,
    key: (&str, Option<crate::core::FieldType>),
) -> Result<Vec<(SqlValue, T)>, ExecError>
where
    E: SqlExecutor<'c>,
    T: MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    select_rows_mapped_on(exec, query, Some(key), |k, t| (k, t)).await
}

/// Decode each row and its `key` cell (`Null` without one) through `out`.
async fn select_rows_mapped_on<'c, T, E, U>(
    exec: E,
    query: &SelectQuery,
    key: Option<(&str, Option<crate::core::FieldType>)>,
    out: fn(SqlValue, T) -> U,
) -> Result<Vec<U>, ExecError>
where
    E: SqlExecutor<'c>,
    U: Send,
    T: MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    let stmt = exec.dialect().compile_select(query)?;
    // Each JOIN target is stitched from the same row, once per FK chain
    // (from its leaf alias). No joins: plain decode.
    let leaves = select_related_leaves(&query.select_related);
    let leaves = &leaves;
    run_on(exec, &stmt.sql, stmt.params, |row| match row {
        #[cfg(feature = "postgres")]
        RowRef::Postgres(row) => {
            let mut t = <T as sqlx::FromRow<PgRow>>::from_row(row)?;
            for &(alias, first_hop) in leaves {
                let _ = t.__rustango_load_related(row, alias, first_hop)?;
            }
            Ok(out(key_cell(row, key)?, t))
        }
        #[cfg(feature = "mysql")]
        RowRef::Mysql(row) => {
            let mut t = <T as sqlx::FromRow<sqlx::mysql::MySqlRow>>::from_row(row)?;
            for &(alias, first_hop) in leaves {
                let _ = t.__rustango_load_related_my(row, alias, first_hop)?;
            }
            Ok(out(key_cell(row, key)?, t))
        }
        #[cfg(feature = "sqlite")]
        RowRef::Sqlite(row) => {
            let mut t = <T as sqlx::FromRow<sqlx::sqlite::SqliteRow>>::from_row(row)?;
            for &(alias, first_hop) in leaves {
                let _ = t.__rustango_load_related_sqlite(row, alias, first_hop)?;
            }
            Ok(out(key_cell(row, key)?, t))
        }
    })
    .await
}

/// The `key` column of `row`; `Null` when there is none.
fn key_cell<R: decode::DecodeRow>(
    row: &R,
    key: Option<(&str, Option<crate::core::FieldType>)>,
) -> Result<SqlValue, sqlx::Error>
where
    for<'a> &'a str: sqlx::ColumnIndex<R>,
{
    key.map_or(Ok(SqlValue::Null), |(name, ty)| {
        decode::named_cell(row, name, ty)
    })
}

/// Gives `QuerySet` a `fetch(&Pool)` method that works on any
/// backend. `select_related` joins are decoded for you.
pub trait FetcherPool<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Compile the queryset and fetch every matching row.
    ///
    /// # Errors
    /// [`ExecError`] for schema, SQL-writing, or driver failures.
    fn fetch(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<Vec<T>, ExecError>> + Send;
}

impl<T> FetcherPool<T> for QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    async fn fetch(self, pool: &Pool) -> Result<Vec<T>, ExecError> {
        let select = self.compile()?;
        select_rows_pool_with_related(pool, &select).await
    }
}

// `_on` twins of the pool methods: run on a held `PoolConn`, a `PoolTx`,
// or any Postgres sqlx executor.
impl<T> QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// [`FetcherPool::fetch`] on a [`SqlExecutor`], e.g. a connection
    /// from [`Pool::acquire`] shared by a handler's queries. On Postgres
    /// it also takes `Tenant::conn()`, which holds the tenant's `search_path`.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`].
    pub async fn fetch_on<'c, E: SqlExecutor<'c>>(self, exec: E) -> Result<Vec<T>, ExecError> {
        let select = self.compile()?;
        select_rows_with_related_on(exec, &select).await
    }

    /// [`Self::first`] on a [`SqlExecutor`].
    ///
    /// # Errors
    /// As [`Self::first`].
    pub async fn first_on<'c, E: SqlExecutor<'c>>(self, exec: E) -> Result<Option<T>, ExecError> {
        let qs = ensure_pk_ordering(self, /*reverse=*/ false);
        Ok(qs.limit(1).fetch_on(exec).await?.into_iter().next())
    }
}

impl<T: Model + Send> QuerySet<T> {
    /// [`CounterPool::count`] on a [`SqlExecutor`].
    ///
    /// # Errors
    /// As [`CounterPool::count`].
    pub async fn count_on<'c, E: SqlExecutor<'c>>(self, exec: E) -> Result<i64, ExecError> {
        let Some(select) = self.compile_unless_none()? else {
            return Ok(0);
        };
        count_rows_on(exec, &CountQuery::from_select(select)).await
    }

    /// [`ExistsPool::exists`] on a [`SqlExecutor`].
    ///
    /// # Errors
    /// As [`ExistsPool::exists`].
    pub async fn exists_on<'c, E: SqlExecutor<'c>>(self, exec: E) -> Result<bool, ExecError> {
        let Some(select) = self.compile_unless_none()? else {
            return Ok(false);
        };
        Ok(count_rows_on(exec, &CountQuery::exists(select)).await? > 0)
    }
}

// Single-row sugar over `FetcherPool`. Each method sets an
// `order_by`, adds `limit(1)` and forwards to `fetch`.
impl<T> crate::query::QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Fetch the first row by the current ordering, or `None` when
    /// nothing matches. With no `order_by`, it sorts by primary key
    /// ASC, so the result is stable.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`].
    pub async fn first(self, pool: &Pool) -> Result<Option<T>, ExecError> {
        let qs = ensure_pk_ordering(self, /*reverse=*/ false);
        let rows = qs.limit(1).fetch(pool).await?;
        Ok(rows.into_iter().next())
    }

    /// Fetch the last row by the current ordering, or `None` when
    /// nothing matches. It flips every sort direction and takes the
    /// first row, so no `OFFSET COUNT(*) - 1` is needed. With no
    /// `order_by`, it sorts by primary key DESC. On a sliced queryset
    /// (`limit` / `offset`) it reads the slice and returns its last row.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`].
    pub async fn last(self, pool: &Pool) -> Result<Option<T>, ExecError> {
        // A non-empty tail ends at the table's last row: two bounded reads.
        if self.is_offset_only() {
            if self.clone().limit(1).fetch(pool).await?.is_empty() {
                return Ok(None);
            }
            let qs = ensure_pk_ordering(self.without_offset(), /*reverse=*/ true);
            return Ok(qs.limit(1).fetch(pool).await?.into_iter().next());
        }
        // Flipping would move the window to the other end of the table.
        if self.is_sliced() {
            let qs = ensure_pk_ordering(self, /*reverse=*/ false);
            return Ok(qs.fetch(pool).await?.pop());
        }
        let qs = ensure_pk_ordering(self, /*reverse=*/ true);
        let rows = qs.limit(1).fetch(pool).await?;
        Ok(rows.into_iter().next())
    }

    /// Fetch the row with the smallest non-NULL `field`, or `None` when
    /// nothing matches. It replaces any `order_by` already set.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`].
    pub async fn earliest(self, field: &str, pool: &Pool) -> Result<Option<T>, ExecError> {
        self.end_by(field, false, pool).await
    }

    /// Fetch the row with the largest non-NULL `field`, or `None` when
    /// nothing matches. It replaces any `order_by` already set.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`].
    pub async fn latest(self, field: &str, pool: &Pool) -> Result<Option<T>, ExecError> {
        self.end_by(field, true, pool).await
    }

    /// One end of `field`. NULLs are skipped, as each backend sorts them
    /// to a different end.
    async fn end_by(self, field: &str, desc: bool, pool: &Pool) -> Result<Option<T>, ExecError> {
        let rows = self
            .filter(&format!("{field}__isnull"), false)
            .replace_order_by(&[(field, desc)])
            .limit(1)
            .fetch(pool)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Fetch the row with this PK that also matches the queryset's
    /// filters, or `None`. Unlike `Model::find(pk, &pool)`, the
    /// filters already on the queryset still apply.
    ///
    /// ```ignore
    /// let post = Post::objects()
    ///     .filter("published", true)
    ///     .find(42_i64, &pool).await?;
    /// // Some(post) only when row 42 is also published.
    /// ```
    ///
    /// A model with no primary key gives
    /// [`ExecError::Query(QueryError::UnknownField)`].
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`].
    pub async fn find(self, pk: impl Into<SqlValue>, pool: &Pool) -> Result<Option<T>, ExecError> {
        self.where_key(pk).first(pool).await
    }

    /// [`Self::first`], but an empty result is an error instead of
    /// `None`. The queryset's filters still apply, so it fails only
    /// when nothing matches them.
    ///
    /// # Errors
    /// As [`Self::first`]; additionally
    /// [`ExecError::Driver(sqlx::Error::RowNotFound)`] when the
    /// queryset returns no rows.
    pub async fn first_or_fail(self, pool: &Pool) -> Result<T, ExecError> {
        match self.first(pool).await? {
            Some(row) => Ok(row),
            None => Err(ExecError::Driver(sqlx::Error::RowNotFound)),
        }
    }

    /// [`Self::find`], but a miss is an error instead of `None`.
    ///
    /// ```ignore
    /// let post = Post::objects()
    ///     .filter("published", true)
    ///     .find_or_fail(42_i64, &pool).await?;
    /// // -> Ok(post) when row 42 is published;
    /// //    Err(RowNotFound) when missing or not published.
    /// ```
    ///
    /// # Errors
    /// As [`Self::first_or_fail`].
    pub async fn find_or_fail(self, pk: impl Into<SqlValue>, pool: &Pool) -> Result<T, ExecError> {
        self.where_key(pk).first_or_fail(pool).await
    }

    /// Fetch the one row matching this queryset. It is an error if
    /// no row matches ([`sqlx::Error::RowNotFound`]) or if more than
    /// one does ([`ExecError::MultipleRowsReturned`]). Uses `LIMIT 2`,
    /// so it does not scan the whole result.
    ///
    /// ```ignore
    /// let post = Post::objects()
    ///     .filter("slug", "hello-world".to_string())
    ///     .sole(&pool).await?;
    /// ```
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`]; additionally
    /// [`ExecError::Driver(sqlx::Error::RowNotFound)`] on empty
    /// result, [`ExecError::MultipleRowsReturned`] on >1 matches.
    pub async fn sole(self, pool: &Pool) -> Result<T, ExecError> {
        let mut rows = self.limit(2).fetch(pool).await?;
        match rows.len() {
            0 => Err(ExecError::Driver(sqlx::Error::RowNotFound)),
            1 => Ok(rows.remove(0)),
            n => Err(ExecError::MultipleRowsReturned {
                op: "sole",
                table: T::SCHEMA.name,
                count: n,
            }),
        }
    }

    /// The largest row by the column in
    /// `#[rustango(get_latest_by = "<col>")]`. Use [`Self::latest`]
    /// to name the field yourself.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`]; also an error when the model does
    /// not declare `get_latest_by`.
    pub async fn latest_default(self, pool: &Pool) -> Result<Option<T>, ExecError> {
        let Some((field, attr_desc)) = T::SCHEMA.get_latest_by else {
            return Err(ExecError::Driver(sqlx::Error::Configuration(
                ::std::format!(
                    "`{model}::latest_default()` requires `#[rustango(get_latest_by = \"<col>\")]`",
                    model = T::SCHEMA.name
                )
                .into(),
            )));
        };
        // The attribute only names the column; `.latest()` is always
        // the descending end.
        let _ = attr_desc;
        self.latest(field, pool).await
    }

    /// The other end of [`Self::latest_default`].
    ///
    /// # Errors
    /// As [`Self::latest_default`].
    pub async fn earliest_default(self, pool: &Pool) -> Result<Option<T>, ExecError> {
        let Some((field, _attr_desc)) = T::SCHEMA.get_latest_by else {
            return Err(ExecError::Driver(sqlx::Error::Configuration(
                ::std::format!(
                    "`{model}::earliest_default()` requires `#[rustango(get_latest_by = \"<col>\")]`",
                    model = T::SCHEMA.name
                )
                .into(),
            )));
        };
        self.earliest(field, pool).await
    }

    /// Read the results
    /// `chunk_size` rows at a time with `LIMIT N OFFSET M`, so a huge
    /// export never has to fit in memory. The queryset is compiled
    /// here, so a schema error surfaces before the first chunk.
    ///
    /// ```ignore
    /// // Two iteration styles, both work:
    /// let mut iter = Post::objects()
    ///     .where_(Post::published.eq(true))
    ///     .order_by(&[("id", false)])
    ///     .iterator(2_000)?;
    ///
    /// // 1. Whole-chunk loop:
    /// while let Some(chunk) = iter.next_chunk(&pool).await? {
    ///     for post in chunk { /* … */ }
    /// }
    ///
    /// // 2. Row-by-row loop (buffer one chunk internally):
    /// while let Some(post) = iter.next_row(&pool).await? {
    ///     /* … */
    /// }
    /// ```
    ///
    /// **Set an order.** Without a stable sort, `OFFSET` returns
    /// unpredictable rows across chunks. Call
    /// `.order_by(&[("pk", …)])` first. This is not enforced, since
    /// some drains do not care about order.
    ///
    /// **Cost.** Each chunk re-runs the query with a larger offset,
    /// and the database scans the skipped rows every time, so deep
    /// paging does O(n²) work. For real streaming on Postgres, use
    /// `transaction()` with sqlx's `fetch(...)` Stream API. This
    /// chunker is the simple option that works on every backend.
    ///
    /// **Writes can skew the result.** Each chunk is its own query,
    /// so a row inserted ahead of the offset can be missed, and a
    /// delete can shift a row into the next chunk and return it
    /// twice. The chunker only takes a `&Pool`, never a transaction.
    /// On a table other writers touch, write the LIMIT/OFFSET loop
    /// yourself against [`select_rows_on`] inside a repeatable-read
    /// transaction. Read-only or append-only tables are fine.
    ///
    /// **`select_for_update()` is lost.** Each chunk runs in its own
    /// implicit transaction, so row locks are released between
    /// chunks. For a locked drain, either use `.fetch_on(&mut *tx)`
    /// when the result fits in memory, or run your own LIMIT/OFFSET
    /// loop inside the transaction.
    ///
    /// # Errors
    /// [`QueryError`](crate::core::QueryError) if the queryset fails
    /// to compile.
    ///
    /// # Panics
    /// If `chunk_size <= 0`. Such a size would silently yield no
    /// rows, which is almost always a bug.
    pub fn iterator(self, chunk_size: i64) -> Result<ChunkedIter<T>, crate::core::QueryError> {
        assert!(
            chunk_size > 0,
            "QuerySet::iterator: chunk_size must be > 0; got {chunk_size}"
        );
        let query = self.compile()?;
        // Chunks page within the queryset's own window.
        let (limit, offset) = if query.compound.is_empty() {
            (query.limit, query.offset)
        } else {
            (query.compound_limit, query.compound_offset)
        };
        let offset = offset.unwrap_or(0).max(0);
        Ok(ChunkedIter {
            query,
            chunk_size,
            offset,
            end: limit.map(|n| offset.saturating_add(n.max(0))),
            exhausted: false,
            buffer: std::collections::VecDeque::new(),
            seen: 0,
            _model: std::marker::PhantomData,
        })
    }

    /// Fetch rows
    /// by a list of column values and return them in a `HashMap`
    /// keyed by that column.
    ///
    /// `column` is a typed [`crate::core::Column`], so the compiler
    /// checks it against the model. The SQL becomes
    /// `… WHERE <column> IN (…)`. `extract` reads the key off each
    /// fetched row, which also lets you choose how to unwrap
    /// `Auto<T>` or `ForeignKey<T, K>`.
    ///
    /// An empty `ids` returns an empty map and runs no SQL.
    ///
    /// ```ignore
    /// use std::collections::HashMap;
    /// use rustango::sql::Auto;
    ///
    /// // Keyed by the Auto<i64> PK. A fetched row is always Set.
    /// let books: HashMap<i64, Book> = Book::objects()
    ///     .in_bulk(Book::id, [1_i64, 2, 3], |b| match b.id {
    ///         Auto::Set(v) => v,
    ///         Auto::Unset  => unreachable!("fetched row has PK"),
    ///     }, &pool)
    ///     .await?;
    ///
    /// // Or key by any other unique column.
    /// let books_by_isbn: HashMap<String, Book> = Book::objects()
    ///     .in_bulk(Book::isbn, ["isbn-1", "isbn-2"], |b| b.isbn.clone(), &pool)
    ///     .await?;
    /// ```
    ///
    /// If two rows share a key, which only happens on a non-unique
    /// column, the later row wins. Prefer a unique column.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`];
    /// [`ExecError::InListUnsplittable`] when `ids` exceed one `IN` list
    /// and the queryset has a limit or offset, or binds of its own fill the cap.
    pub async fn in_bulk<C, K, I, F>(
        self,
        column: C,
        ids: I,
        extract: F,
        pool: &Pool,
    ) -> Result<std::collections::HashMap<K, T>, ExecError>
    where
        C: crate::core::Column<Model = T>,
        K: Eq + std::hash::Hash + Into<crate::core::SqlValue>,
        I: IntoIterator<Item = K>,
        F: Fn(&T) -> K,
    {
        // `column` only carries the `Column<Model = T>` bound; the
        // filter below uses its field `NAME`.
        let _ = column;
        let id_values: Vec<crate::core::SqlValue> = ids.into_iter().map(|v| v.into()).collect();
        if id_values.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = fetch_in_chunks(pool, &self, id_values, |keys| {
            self.clone()
                .filter_op(
                    C::NAME,
                    crate::core::Op::In,
                    crate::core::SqlValue::List(keys),
                )
                .fetch(pool)
        })
        .await?
        // Keyed into a map, so a row two lists return lands once.
        .rows();
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let key = extract(&row);
            out.insert(key, row);
        }
        Ok(out)
    }
}

mod get_or_create;
pub use get_or_create::{get_or_create, get_or_create_tx, update_or_create};

/// Give the queryset a deterministic order before taking one row.
/// An existing `order_by` is kept, or flipped when `reverse`;
/// otherwise the model's primary key is used.
fn ensure_pk_ordering<T: Model>(
    qs: crate::query::QuerySet<T>,
    reverse: bool,
) -> crate::query::QuerySet<T> {
    if !qs.has_order_by() {
        let pk = T::SCHEMA.primary_key().map(|f| f.name);
        if let Some(pk_name) = pk {
            return qs.replace_order_by(&[(pk_name, reverse)]);
        }
        // No PK: leave the order empty and take whatever row the
        // dialect returns first.
        qs
    } else if reverse {
        qs.flip_order_by()
    } else {
        qs
    }
}

/// [`FetcherPool`] for an open transaction, so a read and the writes
/// that follow it share one transaction.
pub trait FetcherTx<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Compile the queryset and fetch every matching row inside the
    /// open transaction.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch`].
    fn fetch_tx(
        self,
        tx: &mut PoolTx<'_>,
    ) -> impl std::future::Future<Output = Result<Vec<T>, ExecError>> + Send;
}

impl<T> FetcherTx<T> for QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    async fn fetch_tx(self, tx: &mut PoolTx<'_>) -> Result<Vec<T>, ExecError> {
        let select = self.compile()?;
        select_rows_tx_with_related(tx, &select).await
    }
}

mod iter;
pub use iter::ChunkedIter;

#[cfg(test)]
mod in_chunk_tests {
    use super::*;

    #[test]
    fn integer_widths_dedupe_as_one_key() {
        assert_eq!(dedupe_key(&SqlValue::I32(1)), dedupe_key(&SqlValue::I64(1)));
        assert_eq!(dedupe_key(&SqlValue::I16(1)), dedupe_key(&SqlValue::I64(1)));
        assert_ne!(dedupe_key(&SqlValue::I64(1)), dedupe_key(&SqlValue::I64(2)));
    }

    #[test]
    fn a_query_that_cannot_be_split_is_refused() {
        assert_eq!(in_chunk_size("t", 10, 10, true).unwrap(), 10);
        assert!(matches!(
            in_chunk_size("t", 11, 10, true),
            Err(ExecError::InListUnsplittable {
                keys: 11,
                max: 10,
                ..
            })
        ));
        assert_eq!(in_chunk_size("t", 11, 10, false).unwrap(), 10);
        assert!(matches!(
            in_chunk_size("t", 5, 0, false),
            Err(ExecError::InListUnsplittable { max: 0, .. })
        ));
    }

    #[test]
    fn rows_per_batch_fits_the_bind_cap_and_the_row_cap() {
        let pg = &crate::sql::Postgres;
        assert_eq!(rows_per_batch(pg, 6, 100), 100);
        assert_eq!(rows_per_batch(pg, 6, usize::MAX), 65535 / 6);
        assert_eq!(
            rows_per_batch(&crate::sql::Sqlite, 3, usize::MAX),
            32766 / 3
        );
        // A row wider than the cap still gets a batch of its own.
        assert_eq!(rows_per_batch(pg, 70_000, usize::MAX), 1);
        assert_eq!(rows_per_batch(pg, 0, 0), 1);
    }
}

#[cfg(test)]
mod last_per_conflict_key_tests {
    use super::*;
    use crate::core::{ConflictClause, FieldSchema, FieldType, ModelSchema};

    const ROW: &ModelSchema = &{
        const FIELDS: &[FieldSchema] = &[
            FieldSchema::new("a", "a", FieldType::I64),
            FieldSchema::new("b", "b", FieldType::I64),
            FieldSchema::new("v", "v", FieldType::I64),
        ];
        let mut s = ModelSchema::new("UpsertRow", "upsert_row");
        s.fields = FIELDS;
        s
    };

    /// An upsert of `(a, b, v)` rows on the `target` key.
    fn upsert(target: &[&'static str], rows: Vec<[SqlValue; 3]>) -> BulkInsertQuery {
        let mut q = BulkInsertQuery::new(
            ROW,
            vec!["a", "b", "v"],
            rows.into_iter().map(Vec::from).collect(),
        );
        q.on_conflict = Some(ConflictClause::DoUpdate {
            target: target.to_vec(),
            update_columns: vec!["v"],
        });
        q
    }

    fn kept(q: &BulkInsertQuery) -> Vec<SqlValue> {
        q.last_per_conflict_key()
            .rows
            .iter()
            .map(|r| r[2].clone())
            .collect()
    }

    fn row(a: Option<i64>, b: i64, v: i64) -> [SqlValue; 3] {
        [
            a.map_or(SqlValue::Null, SqlValue::I64),
            SqlValue::I64(b),
            SqlValue::I64(v),
        ]
    }

    #[test]
    fn null_keys_never_collapse() {
        let q = upsert(
            &["a"],
            vec![row(None, 1, 1), row(None, 1, 2), row(Some(1), 1, 3)],
        );
        assert_eq!(
            kept(&q),
            [SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)]
        );
        // A composite key with one NULL part is no key either.
        let q = upsert(&["a", "b"], vec![row(None, 1, 1), row(None, 1, 2)]);
        assert_eq!(kept(&q), [SqlValue::I64(1), SqlValue::I64(2)]);
    }

    #[test]
    fn a_composite_key_matches_on_every_part() {
        let q = upsert(
            &["a", "b"],
            vec![row(Some(1), 1, 1), row(Some(1), 2, 2), row(Some(1), 1, 3)],
        );
        assert_eq!(kept(&q), [SqlValue::I64(2), SqlValue::I64(3)]);
    }

    /// The repeat is resolved before chunking: the earlier row is gone from
    /// the first batch, not left to clash with the later one in another.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_key_repeated_across_a_chunk_boundary_keeps_the_last() {
        let d = &crate::sql::Sqlite;
        let per_batch = i64::try_from(rows_per_batch(d, 3, usize::MAX)).unwrap();
        let mut rows: Vec<[SqlValue; 3]> = (0..per_batch).map(|i| row(Some(i), 0, i)).collect();
        rows.push(row(Some(0), 0, -1));
        let batches = compile_bulk_insert_batches(d, &upsert(&["a", "b"], rows)).unwrap();
        let params: Vec<&SqlValue> = batches.iter().flat_map(|s| &s.params).collect();
        assert_eq!(params.len(), usize::try_from(per_batch).unwrap() * 3);
        assert_eq!(
            params[..3],
            [&SqlValue::I64(1), &SqlValue::I64(0), &SqlValue::I64(1)]
        );
        assert_eq!(params[params.len() - 1], &SqlValue::I64(-1));
    }
}

#[cfg(test)]
mod pool_dispatch_tests {
    // All inner tests are `#[cfg(feature = "mysql")]` gated; without
    // that feature the imports show as unused.
    #[allow(unused_imports)]
    use super::*;

    /// A `Pool::Mysql` must pick the MySQL dialect, so the SQL it
    /// ships uses backticks and `?`. No live DB needed.
    #[cfg(feature = "mysql")]
    #[tokio::test]
    async fn mysql_pool_dispatch_uses_mysql_dialect() {
        let my = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect_lazy("mysql://user:pass@localhost:1/none")
            .unwrap();
        let pool: Pool = my.into();
        // Guards against a refactor hard-coding Postgres.
        assert_eq!(pool.dialect().name(), "mysql");
        assert_eq!(pool.dialect().quote_ident("col"), "`col`");
        assert_eq!(pool.dialect().placeholder(1), "?");
    }

    /// The same for Postgres: both arms of the dispatch are reachable.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn postgres_pool_dispatch_uses_postgres_dialect() {
        let pg = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap();
        let pool: Pool = pg.into();
        assert_eq!(pool.dialect().name(), "postgres");
        assert_eq!(pool.dialect().quote_ident("col"), "\"col\"");
        assert_eq!(pool.dialect().placeholder(1), "$1");
    }

    /// Compile-time guard for the `MaybeMyFromRow` blanket impl.
    /// `()` implements sqlx's `FromRow<R>` for any `R`, so it is the
    /// safest probe under either feature set.
    #[test]
    fn maybe_my_from_row_resolves_for_unit_type() {
        fn check<T: super::MaybeMyFromRow>() {}
        check::<()>();
    }

    /// `select_related_leaves` reduces join aliases to the deepest in
    /// each FK chain, paired with the first-hop alias.
    #[test]
    fn select_related_leaves_keeps_deepest_chain_aliases() {
        // Single-hop FKs come back as (alias, alias).
        assert_eq!(
            super::select_related_leaves(&["author", "editor"]),
            vec![("author", "author"), ("editor", "editor")],
        );
        // A multi-hop chain emits a join per hop; only the leaf survives,
        // paired with the first hop (where the base FK lives).
        assert_eq!(
            super::select_related_leaves(&[
                "author",
                "author__profile",
                "author__profile__country"
            ]),
            vec![("author__profile__country", "author")],
        );
        // A single-hop FK alongside a chain.
        assert_eq!(
            super::select_related_leaves(&["editor", "author", "author__profile"]),
            vec![("editor", "editor"), ("author__profile", "author")],
        );
        // A shared prefix without a `__` boundary keeps both:
        // `authorship` is not a child of `author`.
        assert_eq!(
            super::select_related_leaves(&["author", "authorship"]),
            vec![("author", "author"), ("authorship", "authorship")],
        );
    }
}
