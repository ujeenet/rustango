//! Dialect-neutral query IR.
//!
//! The query crate compiles a typed `QuerySet<T>` into a [`SelectQuery`].
//! The SQL crate then walks that IR and writes a parameterized statement
//! per dialect. Anything in this module is therefore visible to both.

use crate::{validate::validate_value, ModelSchema, QueryError, SqlValue};

/// Comparison operator on a single column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    /// Right-hand side must be `SqlValue::List`.
    In,
    /// Case-sensitive `LIKE`. Pattern characters live inside the bound value.
    Like,
    /// Compares against `NULL`. The bound value must be `SqlValue::Bool` —
    /// `true` means `IS NULL`, `false` means `IS NOT NULL`.
    IsNull,
}

/// One predicate in a `WHERE` clause: `column <op> value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: &'static str,
    pub op: Op,
    pub value: SqlValue,
}

/// Compiled `SELECT` over a single model with zero or more `AND`-joined filters.
///
/// v0.1 selects all scalar fields of `model` and joins filters with `AND`.
/// `OR` and explicit projections land in v0.2.
///
/// `limit` and `offset` are `None` by default and emit no clauses.
/// `search`, when present, adds a parenthesized `(col ILIKE $N OR …)`
/// clause AND-joined with `filters`. `joins` adds `LEFT JOIN` clauses
/// and pulls extra columns into the projection under aliased names.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub model: &'static ModelSchema,
    pub filters: Vec<Filter>,
    pub search: Option<SearchClause>,
    pub joins: Vec<Join>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// A `LEFT JOIN` against a target model.
///
/// The writer emits `LEFT JOIN "<target.table>" AS "<alias>" ON
/// "<main>"."<on_local>" = "<alias>"."<on_remote>"`, and includes each
/// `project` column in the SELECT list aliased as
/// `"<alias>"."<col>" AS "<alias>__<col>"`. Callers read joined values
/// from the resulting `PgRow` by the suffixed name.
///
/// When a `SelectQuery` has any joins, the writer also qualifies the
/// main table's columns as `"<table>"."<col>"` to avoid ambiguity.
#[derive(Debug, Clone)]
pub struct Join {
    pub target: &'static ModelSchema,
    pub on_local: &'static str,
    pub on_remote: &'static str,
    pub alias: &'static str,
    pub project: Vec<&'static str>,
}

/// `(col1 ILIKE %q% OR col2 ILIKE %q% …)` — single-parameter case-insensitive
/// substring match across multiple columns. Used by the admin's `?q=…` box.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchClause {
    /// SQL columns to search across. Empty = no clause emitted.
    pub columns: Vec<&'static str>,
    /// User-supplied query text. The writer wraps it in `%…%` for `ILIKE`.
    pub query: String,
}

/// Compiled `INSERT` of a single row.
///
/// `columns` and `values` are positional: `values[i]` binds to `columns[i]`.
/// `returning` names columns the writer should append after `RETURNING` —
/// used for `Auto<T>` PKs, where the row is inserted with the column
/// omitted so Postgres' sequence DEFAULT fires, and the assigned value
/// is then read back into the model.
#[derive(Debug, Clone)]
pub struct InsertQuery {
    pub model: &'static ModelSchema,
    pub columns: Vec<&'static str>,
    pub values: Vec<SqlValue>,
    /// Columns to emit in a `RETURNING` clause. Empty = no clause; the
    /// executor uses `execute()`. Non-empty = the executor uses
    /// `fetch_one()` and the caller reads the returned row.
    pub returning: Vec<&'static str>,
}

impl InsertQuery {
    /// Walk each `(column, value)` pair and check it against the field's
    /// declared bounds (`max_length`, `min`, `max`).
    ///
    /// # Errors
    /// Returns [`QueryError::MaxLengthExceeded`] or [`QueryError::OutOfRange`]
    /// for any violating value, or [`QueryError::UnknownField`] if a column
    /// in the IR doesn't correspond to any field in `model`.
    pub fn validate(&self) -> Result<(), QueryError> {
        for (column, value) in self.columns.iter().zip(self.values.iter()) {
            let field =
                self.model
                    .field_by_column(column)
                    .ok_or_else(|| QueryError::UnknownField {
                        model: self.model.name,
                        field: (*column).to_owned(),
                    })?;
            validate_value(self.model.name, field, value)?;
        }
        Ok(())
    }
}

/// Compiled multi-row `INSERT` — one round-trip for N rows.
///
/// `rows[i]` is positional against `columns`: every row supplies the
/// same column list in the same order. `returning` works the same way
/// as on [`InsertQuery`]; non-empty means the executor uses
/// `fetch_all` and returns one row per input row.
///
/// Mixed-shape inserts (some rows opting a column out via the
/// Postgres `DEFAULT` keyword) are not supported in v0.4 — every row
/// must carry a value for every column. Models with `Auto<T>` PKs
/// can either pass `Auto::Unset` for every row (the macro drops the
/// Auto column from `columns` entirely and the sequence fires) or
/// `Auto::Set(v)` for every row (the column is included with the
/// supplied value). Mixed Set/Unset within one bulk_insert is
/// rejected by the macro at validate time.
#[derive(Debug, Clone)]
pub struct BulkInsertQuery {
    pub model: &'static ModelSchema,
    pub columns: Vec<&'static str>,
    pub rows: Vec<Vec<SqlValue>>,
    pub returning: Vec<&'static str>,
}

impl BulkInsertQuery {
    /// Walk every `(column, value)` pair in every row and check it
    /// against the field's declared bounds.
    ///
    /// # Errors
    /// As [`InsertQuery::validate`].
    pub fn validate(&self) -> Result<(), QueryError> {
        for row in &self.rows {
            for (column, value) in self.columns.iter().zip(row.iter()) {
                let field =
                    self.model
                        .field_by_column(column)
                        .ok_or_else(|| QueryError::UnknownField {
                            model: self.model.name,
                            field: (*column).to_owned(),
                        })?;
                validate_value(self.model.name, field, value)?;
            }
        }
        Ok(())
    }
}

/// One `column = value` pair in an `UPDATE ... SET ...` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: &'static str,
    pub value: SqlValue,
}

/// Compiled `UPDATE`.
///
/// `set` are emitted in order before `WHERE`, so their placeholders come first.
/// An empty `filters` runs an unfiltered update affecting every row — the
/// caller is responsible for that being intentional.
#[derive(Debug, Clone)]
pub struct UpdateQuery {
    pub model: &'static ModelSchema,
    pub set: Vec<Assignment>,
    pub filters: Vec<Filter>,
}

impl UpdateQuery {
    /// Walk each `SET column = value` and check it against the field's
    /// declared bounds. Filters are not checked — they compare against
    /// existing rows, not write targets.
    ///
    /// # Errors
    /// As [`InsertQuery::validate`].
    pub fn validate(&self) -> Result<(), QueryError> {
        for assignment in &self.set {
            let field = self
                .model
                .field_by_column(assignment.column)
                .ok_or_else(|| QueryError::UnknownField {
                    model: self.model.name,
                    field: assignment.column.to_owned(),
                })?;
            validate_value(self.model.name, field, &assignment.value)?;
        }
        Ok(())
    }
}

/// Compiled `DELETE`.
///
/// As with `UpdateQuery`, an empty `filters` deletes every row.
#[derive(Debug, Clone)]
pub struct DeleteQuery {
    pub model: &'static ModelSchema,
    pub filters: Vec<Filter>,
}

/// Compiled `SELECT COUNT(*)` — same shape as a `DeleteQuery` (model +
/// filters); the writer emits `COUNT(*)` projection and no `LIMIT`/`OFFSET`.
#[derive(Debug, Clone)]
pub struct CountQuery {
    pub model: &'static ModelSchema,
    pub filters: Vec<Filter>,
}
