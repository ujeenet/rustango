//! Shared internals for the Eloquent-style shortcuts the
//! `#[derive(Model)]` macro emits on every model
//! (`Model::sum` / `Model::where_any` / `Model::increment_each` /
//! …).
//!
//! Why this exists: before this module, the macro emitted the
//! bodies of `__resolve_col` / `__add_signed_expr` /
//! `__aggregate_one_pool` / `__where_multi` / `__increment_one` /
//! `__increment_all` per model. With N derived models in a crate
//! that meant N near-identical copies in the macro's expansion
//! and N copies of doc-comments to keep in sync. Generic
//! monomorphization makes the binary cost identical either way,
//! but maintenance is much cheaper when the bodies live in one
//! regular Rust file. Each macro-emitted shortcut is now a
//! one-line forwarder to a free function here.
//!
//! These functions are **hidden from the public API** — the
//! `Model::*` methods are the user-facing surface. Callers should
//! never import from this module directly.

use crate::core::{AggregateExpr, AggregateQuery, Expr, Model, QueryError, SqlValue, WhereExpr, F};
use crate::query::{QuerySet, Q};
use crate::sql::executor::{
    fetch_aggregate_pool, FetcherPool as _, MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow,
    MaybeSqliteFromRow, MaybeSqliteLoadRelated, UpdaterPool as _,
};
use crate::sql::{ExecError, LoadRelated, Pool};

/// Resolve a runtime `&str` column name to the SCHEMA-registered
/// `&'static str` for use in `F()` expressions and the strongly-
/// typed builder API.
///
/// Surfaces unknown columns as
/// [`ExecError::Query(QueryError::UnknownField)`] keyed by the
/// model's SCHEMA name. Used by the macro-emitted shortcuts
/// (`Model::value` / `Model::sum` / `Model::increment` /
/// `Model::where_any` / …) — kept in one place so the error
/// message + field-not-found mapping stays consistent.
///
/// # Errors
/// [`ExecError::Query(QueryError::UnknownField)`] when the model
/// has no field with that name.
pub fn resolve_col<T: Model>(col: &str) -> Result<&'static str, ExecError> {
    Ok(T::SCHEMA
        .field(col)
        .ok_or_else(|| {
            ExecError::Query(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: col.to_string(),
            })
        })?
        .column)
}

/// Build a `<col_static> + signed_by` expression. Subtracts when
/// `signed_by` is negative. Used by [`increment_one_pool`] /
/// [`increment_all_pool`] to construct the SET clause without
/// re-doing the `F() + Literal()` boilerplate per-model.
#[must_use]
pub fn add_signed_expr(col_static: &'static str, signed_by: i64) -> Expr {
    F(col_static) + Expr::Literal(SqlValue::I64(signed_by))
}

/// Apply `col = col + by` (signed) on the single row whose primary
/// key equals `this_pk`. Backs the `Model::increment` /
/// `Model::decrement` instance methods.
///
/// `pk_field` is the model's primary-key field name as a `&'static
/// str` so the macro can pass `stringify!(#pk_ident)`.
///
/// # Errors
/// As [`UpdaterPool::execute_pool`]; or
/// [`ExecError::Query(QueryError::UnknownField)`] when `col` does
/// not match any model field.
pub async fn increment_one_pool<T>(
    pk_field: &'static str,
    this_pk: SqlValue,
    col: &str,
    by: i64,
    pool: &Pool,
) -> Result<u64, ExecError>
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
    let col_static = resolve_col::<T>(col)?;
    QuerySet::<T>::default()
        .filter(pk_field, this_pk)
        .update()
        .set_expr(col, add_signed_expr(col_static, by))
        .execute_pool(pool)
        .await
}

/// Apply `col = col + by` (signed) on every row of the table.
/// Backs the bulk `Model::increment_each` /
/// `Model::decrement_each` static methods.
///
/// # Errors
/// As [`increment_one_pool`].
pub async fn increment_all_pool<T>(col: &str, by: i64, pool: &Pool) -> Result<u64, ExecError>
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
    let col_static = resolve_col::<T>(col)?;
    QuerySet::<T>::default()
        .update()
        .set_expr(col, add_signed_expr(col_static, by))
        .execute_pool(pool)
        .await
}

/// Run a single-aggregate query (`SELECT SUM(col)` / `AVG(col)` /
/// `MIN(col)` / `MAX(col)` over every row of the table) and
/// return the scalar. Returns `Ok(None)` when the table is empty
/// (matches SQL's `SUM(empty)` shape).
///
/// `build` is a callback that takes the SCHEMA-resolved column
/// name and returns the `AggregateExpr` to compile — chooses the
/// kind. Used by `Model::sum` / `Model::avg` / `Model::min` /
/// `Model::max` so each one is a single-line wrapper that picks
/// its kind via the closure.
///
/// # Errors
/// As [`fetch_aggregate_pool`].
pub async fn aggregate_one_pool<T, U>(
    col: &str,
    build: fn(&'static str) -> AggregateExpr,
    pool: &Pool,
) -> Result<Option<U>, ExecError>
where
    T: Model,
    (Option<U>,): MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    let col_static = resolve_col::<T>(col)?;
    let q = AggregateQuery {
        model: T::SCHEMA,
        where_clause: WhereExpr::And(Vec::new()),
        group_by: Vec::new(),
        aggregates: vec![("v", build(col_static))],
        aliases: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
    };
    let rows: Vec<(Option<U>,)> = fetch_aggregate_pool(pool, &q).await?;
    Ok(rows.into_iter().next().and_then(|t| t.0))
}

/// Compose `cols` into an OR (`all = false`) or AND (`all = true`)
/// chain of `col = val` predicates and fetch every matching row.
/// Backs `Model::where_any` / `Model::where_all`.
///
/// Empty-cols semantics:
/// - `all = false` → returns no rows (vacuous OR is FALSE).
/// - `all = true`  → returns every row (vacuous AND is TRUE).
///
/// # Errors
/// As [`FetcherPool::fetch_pool`]; or
/// [`ExecError::Query(QueryError::UnknownField)`] when any
/// column is not declared on the model.
pub async fn where_multi_pool<T>(
    cols: &[&str],
    val: impl Into<SqlValue>,
    all: bool,
    pool: &Pool,
) -> Result<Vec<T>, ExecError>
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
    if cols.is_empty() {
        return if all {
            QuerySet::<T>::default().fetch_pool(pool).await
        } else {
            Ok(Vec::new())
        };
    }
    let sql_val: SqlValue = val.into();
    let mut q: Option<Q> = None;
    for col in cols {
        let col_static = resolve_col::<T>(col)?;
        let pred = Q::eq(col_static, sql_val.clone());
        q = Some(match q {
            None => pred,
            Some(prev) => {
                if all {
                    prev & pred
                } else {
                    prev | pred
                }
            }
        });
    }
    QuerySet::<T>::default()
        .where_(q.expect("non-empty cols"))
        .fetch_pool(pool)
        .await
}
