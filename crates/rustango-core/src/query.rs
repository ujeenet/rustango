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

/// One predicate in a `WHERE` clause: `column <op> value`. Always
/// the leaf of a [`WhereExpr`] tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: &'static str,
    pub op: Op,
    pub value: SqlValue,
}

/// Boolean expression in a `WHERE` clause — leaf [`Filter`]s composed
/// with `AND` / `OR` to arbitrary depth.
///
/// ```ignore
/// // a AND (b OR c)
/// WhereExpr::And(vec![
///     WhereExpr::Predicate(a),
///     WhereExpr::Or(vec![WhereExpr::Predicate(b), WhereExpr::Predicate(c)]),
/// ])
/// ```
///
/// Empty conjunctions and disjunctions are valid. By convention they
/// represent SQL `TRUE` and `FALSE` respectively, but you should
/// usually avoid building them — `WhereExpr::And(vec![])` is the
/// "no filters" case used internally to represent a query with an
/// unfiltered WHERE clause; the writer skips emitting `WHERE` for it.
/// `WhereExpr::Or(vec![])` is rejected by the writer as it would
/// silently match nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum WhereExpr {
    /// Leaf — a single column predicate.
    Predicate(Filter),
    /// All children must match. Empty list = vacuously true (no
    /// `WHERE` emitted by the writer).
    And(Vec<WhereExpr>),
    /// Any child must match. Empty list = vacuously false (rejected
    /// by the writer).
    Or(Vec<WhereExpr>),
}

impl WhereExpr {
    /// `true` when this expression carries no predicates (i.e. an
    /// empty `And`). Used by the writer to skip emitting `WHERE`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::And(items) if items.is_empty())
    }

    /// Build an AND of leaf filters. Convenience for the common case
    /// of "a list of predicates joined with AND" (the legacy
    /// `Vec<Filter>` shape).
    #[must_use]
    pub fn and_predicates(filters: Vec<Filter>) -> Self {
        Self::And(filters.into_iter().map(Self::Predicate).collect())
    }

    /// Append an AND predicate. If `self` is already `And(_)`, the
    /// child is pushed in place; otherwise `self` is wrapped in a new
    /// `And` together with the new child.
    pub fn push_and(&mut self, child: Self) {
        match self {
            Self::And(items) => items.push(child),
            _ => {
                let prev = std::mem::replace(self, Self::And(Vec::new()));
                if let Self::And(items) = self {
                    items.push(prev);
                    items.push(child);
                }
            }
        }
    }

    /// If this expression is a flat AND of leaf predicates (or a
    /// single `Predicate`), return the predicate list. Returns `None`
    /// for any tree containing `Or` or nested `And`. Useful for
    /// callers that want to inspect a legacy "AND-only" WHERE without
    /// pattern-matching the full tree.
    #[must_use]
    pub fn as_flat_and(&self) -> Option<Vec<&Filter>> {
        match self {
            Self::Predicate(f) => Some(vec![f]),
            Self::And(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Self::Predicate(f) => out.push(f),
                        _ => return None,
                    }
                }
                Some(out)
            }
            Self::Or(_) => None,
        }
    }

    /// Walk the tree and validate every leaf predicate against `model`.
    ///
    /// # Errors
    /// Returns [`QueryError::UnknownField`] for a predicate whose
    /// column is missing from the model, propagated up through
    /// composite nodes.
    pub fn validate(&self, model: &'static ModelSchema) -> Result<(), QueryError> {
        match self {
            Self::Predicate(f) => {
                if model.field_by_column(f.column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: f.column.to_owned(),
                    });
                }
                Ok(())
            }
            Self::And(items) | Self::Or(items) => {
                for child in items {
                    child.validate(model)?;
                }
                Ok(())
            }
        }
    }
}

impl Default for WhereExpr {
    fn default() -> Self {
        Self::And(Vec::new())
    }
}

impl From<Filter> for WhereExpr {
    fn from(f: Filter) -> Self {
        Self::Predicate(f)
    }
}

/// Compiled `SELECT` over a single model with an optional WHERE
/// clause expressed as a [`WhereExpr`] tree.
///
/// v0.7 ships full AND/OR/nested support. The legacy "flat AND of
/// predicates" shape is `WhereExpr::and_predicates(filters)` for
/// callers who built up a `Vec<Filter>` directly.
///
/// `limit` and `offset` are `None` by default and emit no clauses.
/// `search`, when present, adds a parenthesized `(col ILIKE $N OR …)`
/// clause AND-joined with `where_clause`. `joins` adds `LEFT JOIN`
/// clauses and pulls extra columns into the projection under aliased
/// names.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
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
/// `set` are emitted in order before `WHERE`, so their placeholders
/// come first. An empty `where_clause` (the default `WhereExpr::And(vec![])`)
/// runs an unfiltered update affecting every row — the caller is
/// responsible for that being intentional.
#[derive(Debug, Clone)]
pub struct UpdateQuery {
    pub model: &'static ModelSchema,
    pub set: Vec<Assignment>,
    pub where_clause: WhereExpr,
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
/// As with `UpdateQuery`, an empty `where_clause` deletes every row.
#[derive(Debug, Clone)]
pub struct DeleteQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
}

/// Compiled `SELECT COUNT(*)` — same shape as a `DeleteQuery` (model +
/// where clause); the writer emits `COUNT(*)` projection and no
/// `LIMIT`/`OFFSET`.
#[derive(Debug, Clone)]
pub struct CountQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
}
