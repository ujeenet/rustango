//! Shared internals for the Eloquent-style shortcuts the
//! `#[derive(Model)]` macro emits on every model
//! (`Model::where_any` / `Model::increment_each` / …). Each one
//! builds a `QuerySet`, so the model's global scopes apply.
//!
//! Why this exists: before this module, the macro emitted the
//! bodies of `__resolve_col` / `__add_signed_expr` /
//! `__where_multi` / `__increment_one` /
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

use crate::core::{Expr, Model, QueryError, SqlValue, UpdateQuery, F};
use crate::query::{QuerySet, Q};
use crate::sql::executor::{
    FetcherPool as _, MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow, MaybeSqliteFromRow,
    MaybeSqliteLoadRelated, UpdaterPool as _,
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
/// [`increment_all_query`] to construct the SET clause without
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
/// As [`UpdaterPool::execute_pool`](crate::sql::UpdaterPool::execute_pool);
/// or [`ExecError::Query`] with `QueryError::UnknownField` when `col`
/// does not match any model field.
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

/// `UPDATE … SET col = col + by` (signed) over the whole table. Backs
/// `Model::increment_each` / `decrement_each`.
///
/// # Errors
/// As [`resolve_col`], plus the update compile.
pub fn increment_all_query<T: Model>(col: &str, by: i64) -> Result<UpdateQuery, ExecError> {
    let col_static = resolve_col::<T>(col)?;
    Ok(QuerySet::<T>::default()
        .update()
        .set_expr(col, add_signed_expr(col_static, by))
        .compile()?)
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
/// As [`FetcherPool::fetch`](crate::sql::FetcherPool::fetch); or
/// [`ExecError::Query`] with `QueryError::UnknownField` when any
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
            QuerySet::<T>::default().fetch(pool).await
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
        .fetch(pool)
        .await
}
