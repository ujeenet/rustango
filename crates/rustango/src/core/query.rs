//! Dialect-neutral query IR.
//!
//! The query crate compiles a typed `QuerySet<T>` into a [`SelectQuery`].
//! The SQL crate then walks that IR and writes a parameterized statement
//! per dialect. Anything in this module is therefore visible to both.

use super::expr::Expr;
use super::{validate::validate_value, ModelSchema, QueryError, SqlValue};

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
    /// Right-hand side must be `SqlValue::List`. Emits `NOT IN (…)`.
    NotIn,
    /// Case-sensitive `LIKE`. Pattern characters live inside the bound value.
    Like,
    /// Case-sensitive `NOT LIKE`.
    NotLike,
    /// Case-insensitive `ILIKE` (Postgres).
    ILike,
    /// Case-insensitive `NOT ILIKE` (Postgres).
    NotILike,
    /// Range check. The bound value must be `SqlValue::List([lo, hi])`.
    /// Emits `col BETWEEN $lo AND $hi`.
    Between,
    /// Compares against `NULL`. The bound value must be `SqlValue::Bool` —
    /// `true` means `IS NULL`, `false` means `IS NOT NULL`.
    IsNull,
    /// Null-safe equality: `IS DISTINCT FROM`. Unlike `<>`, this treats
    /// `NULL` as a comparable value — `NULL IS NOT DISTINCT FROM NULL` is
    /// `true`. Bind any `SqlValue`.
    IsDistinctFrom,
    /// Null-safe equality: `IS NOT DISTINCT FROM`. The inverse of
    /// [`IsDistinctFrom`](Op::IsDistinctFrom).
    IsNotDistinctFrom,
    /// JSONB `@>` — left operand contains the right operand. Bind a
    /// `SqlValue::Json` value.
    JsonContains,
    /// JSONB `<@` — left operand is contained by the right operand.
    JsonContainedBy,
    /// JSONB `?` — the text key exists as a top-level key. Bind a
    /// `SqlValue::String`.
    JsonHasKey,
    /// JSONB `?|` — any of the text keys exist. Bind a `SqlValue::List`
    /// of `SqlValue::String`.
    JsonHasAnyKey,
    /// JSONB `?&` — all of the text keys exist. Bind a `SqlValue::List`
    /// of `SqlValue::String`.
    JsonHasAllKeys,
}

/// One predicate in a `WHERE` clause: `column <op> value`. Always
/// the leaf of a [`WhereExpr`] tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: &'static str,
    pub op: Op,
    pub value: SqlValue,
}

/// `WHERE` predicate that compares two columns from the same row —
/// the rustango analog of Django's `F()` on the right side of a filter.
///
/// Emits `<left_col> <op> <right>` where `right` is an arbitrary
/// [`Expr`] (typically `Expr::Column` for a plain column-vs-column
/// compare, or a `BinOp` tree for column-vs-arithmetic). The lhs is
/// the model column being filtered on; the rhs lives in the `Expr`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnFilter {
    /// Left-hand column (the field being filtered on, schema-resolved).
    pub column: &'static str,
    /// Comparison operator. Subset of [`Op`] — only the binary
    /// comparison variants make sense here (`Eq`, `Ne`, `Lt`, `Lte`,
    /// `Gt`, `Gte`). Other ops (`In`, `Between`, `IsNull`, JSON ops, etc.)
    /// are rejected at compile/emit time.
    pub op: Op,
    /// Right-hand side. Most commonly `Expr::Column(other)` for the
    /// column-vs-column case; can be any expression tree.
    pub rhs: Expr,
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
    /// Leaf — a column-vs-expression predicate (F() comparisons).
    ColumnCompare(ColumnFilter),
    /// All children must match. Empty list = vacuously true (no
    /// `WHERE` emitted by the writer).
    And(Vec<WhereExpr>),
    /// Any child must match. Empty list = vacuously false (rejected
    /// by the writer).
    Or(Vec<WhereExpr>),
    /// Logical negation. Emits `NOT (child)`.
    Not(Box<WhereExpr>),
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
            // ColumnCompare is a leaf predicate but it doesn't carry a
            // `Filter` (the rhs is an `Expr`, not a `SqlValue`), so the
            // flat-AND view can't surface it as a `&Filter` reference.
            // Callers using `as_flat_and` only handle literal `Filter`
            // predicates anyway.
            Self::ColumnCompare(_) | Self::Or(_) | Self::Not(_) => None,
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
            Self::ColumnCompare(cf) => {
                if model.field_by_column(cf.column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: cf.column.to_owned(),
                    });
                }
                // Validate every column reference inside the rhs Expr
                // tree against the model schema.
                validate_expr_columns(model, &cf.rhs)?;
                Ok(())
            }
            Self::And(items) | Self::Or(items) => {
                for child in items {
                    child.validate(model)?;
                }
                Ok(())
            }
            Self::Not(child) => child.validate(model),
        }
    }
}

/// Recursively walk an [`Expr`] and confirm every `Column` reference
/// resolves on `model`. Literals + arithmetic ops are passed through.
fn validate_expr_columns(model: &'static ModelSchema, expr: &Expr) -> Result<(), QueryError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(name) => {
            if model.field_by_column(name).is_none() {
                Err(QueryError::UnknownField {
                    model: model.name,
                    field: (*name).to_owned(),
                })
            } else {
                Ok(())
            }
        }
        Expr::BinOp { left, right, .. } => {
            validate_expr_columns(model, left)?;
            validate_expr_columns(model, right)
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
    /// `ORDER BY` clauses, in the order they should appear in SQL.
    /// Slice 9.0b. Emitted after WHERE / JOIN / GROUP BY but before
    /// LIMIT / OFFSET. Empty = no `ORDER BY` (existing behaviour).
    pub order_by: Vec<OrderClause>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Single column in an `ORDER BY` clause. Slice 9.0b.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderClause {
    /// SQL column name on the main table — already resolved by
    /// `QuerySet::order_by` from a Rust-side field name (so the
    /// writer doesn't re-walk the schema).
    pub column: &'static str,
    /// `true` for `DESC`, `false` for the default `ASC`.
    pub desc: bool,
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
/// Conflict resolution for `INSERT … ON CONFLICT` (Postgres-specific).
///
/// Attach to [`InsertQuery::on_conflict`] or [`BulkInsertQuery::on_conflict`]
/// to control what happens when the insert would violate a unique constraint.
#[derive(Debug, Clone)]
pub enum ConflictClause {
    /// `ON CONFLICT DO NOTHING` — silently skip duplicate rows.
    DoNothing,
    /// `ON CONFLICT (target) DO UPDATE SET col = EXCLUDED.col` for each
    /// column in `update_columns`. `target` names the column(s) whose
    /// uniqueness constraint defines the conflict (typically the PK or a
    /// `#[rustango(unique)]` column).
    DoUpdate {
        target: Vec<&'static str>,
        update_columns: Vec<&'static str>,
    },
}

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
    /// Optional `ON CONFLICT` clause. `None` = plain INSERT with no
    /// conflict handling (errors on constraint violation).
    pub on_conflict: Option<ConflictClause>,
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
    /// Optional `ON CONFLICT` clause applied to every row in the batch.
    pub on_conflict: Option<ConflictClause>,
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
///
/// `value` is an [`Expr`] — it can be a literal (most common, `Expr::Literal`),
/// a column reference (`Expr::Column` / `F("col")` — column-to-column copy),
/// or an arithmetic tree (`F("col") + 1` — Django's atomic counter pattern).
///
/// Existing call sites that pass an [`SqlValue`] lift transparently
/// via `impl From<SqlValue> for Expr` — the field's `Into`-bound public
/// builders (`Column::set`, `UpdateBuilder::set`) keep their original
/// signatures.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: &'static str,
    pub value: Expr,
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
            // Only literal rhs values are checkable against the field's
            // declared bounds; column refs and arithmetic trees don't
            // resolve to a single concrete value at compile time.
            if let Some(literal) = assignment.value.as_literal() {
                validate_value(self.model.name, field, literal)?;
            }
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
    /// Optional ILIKE search across the supplied columns. When set
    /// the count includes only rows that *also* match the search —
    /// without this the page-number list endpoint reported the
    /// wrong total whenever `?search=...` was active.
    pub search: Option<SearchClause>,
}

/// Bulk per-row UPDATE using `UPDATE t SET … FROM (VALUES …)`. One row
/// in the VALUES clause per input item; the PK identifies which table row
/// to update.
///
/// All rows must supply the same `update_columns` list in the same order.
/// The PK column must match `model.primary_key()`.
///
/// Built via [`crate::sql::bulk_update`] or directly.
#[derive(Debug, Clone)]
pub struct BulkUpdateQuery {
    pub model: &'static ModelSchema,
    /// The column names to update (not including the PK).
    pub update_columns: Vec<&'static str>,
    /// One inner `Vec<SqlValue>` per row: `[pk_value, col1_value, col2_value, …]`.
    /// The first element is always the PK; the rest align with `update_columns`.
    pub rows: Vec<Vec<SqlValue>>,
}

/// One aggregate expression in an [`AggregateQuery`].
#[derive(Debug, Clone)]
pub enum AggregateExpr {
    /// `COUNT(*)` or `COUNT(column)` when `column` is `Some`.
    Count(Option<&'static str>),
    /// `COUNT(DISTINCT column)` — counts distinct values in a column.
    /// v0.45. Works on PG / MySQL 8+ / SQLite 3.35+.
    CountDistinct(&'static str),
    /// `SUM(column)`.
    Sum(&'static str),
    /// `AVG(column)`.
    Avg(&'static str),
    /// `MAX(column)`.
    Max(&'static str),
    /// `MIN(column)`.
    Min(&'static str),
}

/// A `SELECT … GROUP BY … HAVING …` query. Returned rows are untyped
/// (`HashMap<String, SqlValue>`) because the projection is dynamic.
///
/// Build via [`crate::query::QuerySet::aggregate`].
#[derive(Debug, Clone)]
pub struct AggregateQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
    /// Columns to group by. Must be valid column names on `model`.
    pub group_by: Vec<&'static str>,
    /// `(alias, expr)` pairs — the alias becomes the key in each result row.
    pub aggregates: Vec<(&'static str, AggregateExpr)>,
    /// Optional HAVING clause (applied after GROUP BY).
    pub having: Option<WhereExpr>,
    pub order_by: Vec<OrderClause>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}
