//! Dialect-agnostic SQL writers.
//!
//! Both `postgres::Postgres` and `mysql::MySql` route their `compile_*`
//! methods through these helpers. Identifier quoting, placeholder
//! shape, NULL-cast emission, and operator translation all dispatch
//! through the [`Dialect`] reference held by [`Sql`], so the writers
//! produce dialect-correct SQL without per-backend forks.
//!
//! Postgres-specific syntax that has no portable equivalent
//! (`ILIKE`, `IS DISTINCT FROM`, JSONB `@>` / `?` / `?|` / `?&`) is
//! gated on [`Dialect::supports_op`] — dialects that return `false`
//! produce a clear [`SqlError::OperatorNotSupportedInDialect`] error
//! instead of mis-translated SQL.

use std::fmt::Write as _;

use crate::core::{
    AggregateExpr, AggregateQuery, BulkInsertQuery, BulkUpdateQuery, CountQuery, DeleteQuery,
    Filter, InsertQuery, ModelSchema, Op, OrderClause, SearchClause, SelectQuery, SqlValue,
    UpdateQuery, WhereExpr,
};

use super::{CompiledStatement, Dialect, SqlError};

/// Buffer-and-params bundle threaded through every writer below. Owns
/// a borrowed [`Dialect`] so each helper can ask for the right
/// identifier quote, parameter placeholder, NULL cast, etc. without
/// branching on backend.
#[allow(clippy::struct_field_names)] // `sql.sql` reads naturally for builder calls
pub(super) struct Sql<'d> {
    pub d: &'d dyn Dialect,
    pub sql: String,
    pub params: Vec<SqlValue>,
}

impl<'d> Sql<'d> {
    pub(super) fn new(d: &'d dyn Dialect) -> Self {
        Self {
            d,
            sql: String::new(),
            params: Vec::new(),
        }
    }

    pub(super) fn with_capacity(d: &'d dyn Dialect, cap: usize) -> Self {
        Self {
            d,
            sql: String::new(),
            params: Vec::with_capacity(cap),
        }
    }

    /// Append a quoted identifier using the dialect's quoting rules.
    pub(super) fn write_ident(&mut self, name: &str) {
        self.sql.push_str(&self.d.quote_ident(name));
    }

    /// Push `value` to the param list and emit the dialect's
    /// placeholder for the new slot. For Postgres + a NULL value, also
    /// emit `::TYPE` when the column type is known — see
    /// [`Dialect::null_cast`].
    pub(super) fn push_param_typed(&mut self, value: SqlValue, cast: Option<&'static str>) {
        let is_null = matches!(value, SqlValue::Null);
        self.params.push(value);
        let p = self.d.placeholder(self.params.len());
        self.sql.push_str(&p);
        if is_null {
            if let Some(ty) = cast {
                self.sql.push_str("::");
                self.sql.push_str(ty);
            }
        }
    }

    /// Same as [`Self::push_param_typed`] without a cast hint — used
    /// for values whose column type the writer can't determine
    /// (e.g. JSON-key list elements).
    pub(super) fn push_param(&mut self, value: SqlValue) {
        self.push_param_typed(value, None);
    }

    pub(super) fn finish(self) -> CompiledStatement {
        CompiledStatement {
            sql: self.sql,
            params: self.params,
        }
    }
}

/// Per-column NULL-cast lookup gated on the dialect. Postgres needs the
/// hint; `MySQL` doesn't and the writers will get `None`.
pub(super) fn null_cast_for(
    d: &dyn Dialect,
    model: &ModelSchema,
    column: &str,
) -> Option<&'static str> {
    let field = model.field_by_column(column)?;
    d.null_cast(field.ty)
}

// ====================================================================
// SELECT
// ====================================================================

pub(super) fn write_select(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    let qualify = !query.joins.is_empty();

    b.sql.push_str("SELECT ");
    let mut first_col = true;
    for field in query.model.scalar_fields() {
        if !first_col {
            b.sql.push_str(", ");
        }
        first_col = false;
        if qualify {
            b.write_ident(query.model.table);
            b.sql.push('.');
        }
        b.write_ident(field.column);
    }
    for join in &query.joins {
        for col in &join.project {
            b.sql.push_str(", ");
            b.write_ident(join.alias);
            b.sql.push('.');
            b.write_ident(col);
            b.sql.push_str(" AS ");
            b.write_ident(&format!("{}__{}", join.alias, col));
        }
    }

    b.sql.push_str(" FROM ");
    b.write_ident(query.model.table);

    for join in &query.joins {
        b.sql.push_str(" LEFT JOIN ");
        b.write_ident(join.target.table);
        b.sql.push_str(" AS ");
        b.write_ident(join.alias);
        b.sql.push_str(" ON ");
        b.write_ident(query.model.table);
        b.sql.push('.');
        b.write_ident(join.on_local);
        b.sql.push_str(" = ");
        b.write_ident(join.alias);
        b.sql.push('.');
        b.write_ident(join.on_remote);
    }

    write_where_with_search(
        b,
        &query.where_clause,
        query.search.as_ref(),
        qualify.then_some(query.model.table),
        Some(query.model),
    )?;

    write_order_limit_offset(
        b,
        &query.order_by,
        query.limit,
        query.offset,
        qualify.then_some(query.model.table),
    );

    Ok(())
}

// ====================================================================
// COUNT
// ====================================================================

pub(super) fn write_count(b: &mut Sql<'_>, query: &CountQuery) -> Result<(), SqlError> {
    b.sql.push_str("SELECT COUNT(*) FROM ");
    b.write_ident(query.model.table);
    write_where_with_search(
        b,
        &query.where_clause,
        query.search.as_ref(),
        None,
        Some(query.model),
    )?;
    Ok(())
}

// ====================================================================
// AGGREGATE
// ====================================================================

pub(super) fn write_aggregate(b: &mut Sql<'_>, query: &AggregateQuery) -> Result<(), SqlError> {
    b.sql.push_str("SELECT ");

    for (i, col) in query.group_by.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        b.write_ident(col);
    }
    for (i, (alias, expr)) in query.aggregates.iter().enumerate() {
        if !query.group_by.is_empty() || i > 0 {
            b.sql.push_str(", ");
        }
        match expr {
            AggregateExpr::Count(None) => b.sql.push_str("COUNT(*)"),
            AggregateExpr::Count(Some(col)) => {
                b.sql.push_str("COUNT(");
                b.write_ident(col);
                b.sql.push(')');
            }
            AggregateExpr::CountDistinct(col) => {
                b.sql.push_str("COUNT(DISTINCT ");
                b.write_ident(col);
                b.sql.push(')');
            }
            AggregateExpr::Sum(col) => {
                // Both PG and MySQL widen SUM(int) into a type the
                // SqlValue aggregate decoder doesn't try (PG NUMERIC,
                // MySQL DECIMAL). Ask the dialect to cast back to a
                // known scalar so the i64 decode arm picks it up.
                let inner = format!("SUM({})", b.d.quote_ident(col));
                let wrapped = b.d.cast_aggregate_to_int(&inner);
                b.sql.push_str(&wrapped);
            }
            AggregateExpr::Avg(col) => {
                let inner = format!("AVG({})", b.d.quote_ident(col));
                let wrapped = b.d.cast_aggregate_to_float(&inner);
                b.sql.push_str(&wrapped);
            }
            AggregateExpr::Max(col) => {
                b.sql.push_str("MAX(");
                b.write_ident(col);
                b.sql.push(')');
            }
            AggregateExpr::Min(col) => {
                b.sql.push_str("MIN(");
                b.write_ident(col);
                b.sql.push(')');
            }
            AggregateExpr::ArrayAgg { column, distinct } => {
                if b.d.name() != "postgres" {
                    return Err(SqlError::AggregateNotSupportedInDialect {
                        aggregate: "array_agg",
                        dialect: b.d.name(),
                    });
                }
                b.sql.push_str("array_agg(");
                if *distinct {
                    b.sql.push_str("DISTINCT ");
                }
                b.write_ident(column);
                b.sql.push(')');
            }
            AggregateExpr::StringAgg {
                column,
                delimiter,
                distinct,
            } => {
                if b.d.name() != "postgres" {
                    return Err(SqlError::AggregateNotSupportedInDialect {
                        aggregate: "string_agg",
                        dialect: b.d.name(),
                    });
                }
                b.sql.push_str("string_agg(");
                if *distinct {
                    b.sql.push_str("DISTINCT ");
                }
                b.write_ident(column);
                b.sql.push_str(", ");
                // Delimiter binds as a parameter — no string interpolation.
                b.push_param(crate::core::SqlValue::String(delimiter.clone()));
                b.sql.push(')');
            }
            AggregateExpr::JsonbAgg { column } => {
                if b.d.name() != "postgres" {
                    return Err(SqlError::AggregateNotSupportedInDialect {
                        aggregate: "jsonb_agg",
                        dialect: b.d.name(),
                    });
                }
                b.sql.push_str("jsonb_agg(");
                b.write_ident(column);
                b.sql.push(')');
            }
        }
        b.sql.push_str(" AS ");
        b.write_ident(alias);
    }

    b.sql.push_str(" FROM ");
    b.write_ident(query.model.table);
    write_where(b, &query.where_clause, Some(query.model))?;

    if !query.group_by.is_empty() {
        b.sql.push_str(" GROUP BY ");
        for (i, col) in query.group_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.write_ident(col);
        }
    }

    if let Some(having) = &query.having {
        b.sql.push_str(" HAVING ");
        write_where_expr(b, having, None, Some(query.model))?;
    }

    write_order_limit_offset(b, &query.order_by, query.limit, query.offset, None);

    Ok(())
}

// ====================================================================
// INSERT
// ====================================================================

pub(super) fn write_insert(b: &mut Sql<'_>, query: &InsertQuery) -> Result<(), SqlError> {
    if query.columns.is_empty() && query.returning.is_empty() {
        return Err(SqlError::EmptyInsert);
    }
    if query.columns.len() != query.values.len() {
        return Err(SqlError::InsertShapeMismatch {
            columns: query.columns.len(),
            values: query.values.len(),
        });
    }

    b.sql.push_str("INSERT INTO ");
    b.write_ident(query.model.table);

    if query.columns.is_empty() {
        b.sql.push_str(" DEFAULT VALUES");
    } else {
        b.sql.push_str(" (");
        let mut first = true;
        for col in &query.columns {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            b.write_ident(col);
        }
        b.sql.push_str(") VALUES (");
        let mut first = true;
        for (col, value) in query.columns.iter().zip(&query.values) {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            let cast = null_cast_for(b.d, query.model, col);
            b.push_param_typed(value.clone(), cast);
        }
        b.sql.push(')');
    }

    if let Some(conflict) = &query.on_conflict {
        b.d.write_conflict_clause(&mut b.sql, conflict)?;
    }

    write_returning(b, &query.returning)?;
    Ok(())
}

// ====================================================================
// BULK INSERT
// ====================================================================

pub(super) fn write_bulk_insert(b: &mut Sql<'_>, query: &BulkInsertQuery) -> Result<(), SqlError> {
    if query.rows.is_empty() {
        return Err(SqlError::EmptyBulkInsert);
    }
    if query.columns.is_empty() && query.returning.is_empty() {
        return Err(SqlError::EmptyInsert);
    }
    for row in &query.rows {
        if row.len() != query.columns.len() {
            return Err(SqlError::InsertShapeMismatch {
                columns: query.columns.len(),
                values: row.len(),
            });
        }
    }

    b.sql.push_str("INSERT INTO ");
    b.write_ident(query.model.table);

    if query.columns.is_empty() {
        let pk = query
            .returning
            .first()
            .copied()
            .ok_or(SqlError::EmptyInsert)?;
        b.sql.push_str(" (");
        b.write_ident(pk);
        b.sql.push_str(") VALUES ");
        let mut first_row = true;
        for _ in &query.rows {
            if !first_row {
                b.sql.push_str(", ");
            }
            first_row = false;
            b.sql.push_str("(DEFAULT)");
        }
    } else {
        b.sql.push_str(" (");
        let mut first = true;
        for col in &query.columns {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            b.write_ident(col);
        }
        b.sql.push_str(") VALUES ");

        let mut first_row = true;
        for row in &query.rows {
            if !first_row {
                b.sql.push_str(", ");
            }
            first_row = false;
            b.sql.push('(');
            let mut first_v = true;
            for (col, value) in query.columns.iter().zip(row) {
                if !first_v {
                    b.sql.push_str(", ");
                }
                first_v = false;
                let cast = null_cast_for(b.d, query.model, col);
                b.push_param_typed(value.clone(), cast);
            }
            b.sql.push(')');
        }
    }

    if let Some(conflict) = &query.on_conflict {
        b.d.write_conflict_clause(&mut b.sql, conflict)?;
    }

    write_returning(b, &query.returning)?;
    Ok(())
}

// ====================================================================
// UPDATE
// ====================================================================

pub(super) fn write_update(b: &mut Sql<'_>, query: &UpdateQuery) -> Result<(), SqlError> {
    if query.set.is_empty() {
        return Err(SqlError::EmptyUpdateSet);
    }

    b.sql.push_str("UPDATE ");
    b.write_ident(query.model.table);
    b.sql.push_str(" SET ");

    let mut first = true;
    for assignment in &query.set {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        b.write_ident(assignment.column);
        b.sql.push_str(" = ");
        let cast = null_cast_for(b.d, query.model, assignment.column);
        write_expr(b, &assignment.value, cast)?;
    }

    write_where(b, &query.where_clause, Some(query.model))?;
    Ok(())
}

/// Render a [`crate::core::Expr`] — the recursive RHS form that
/// powers `F()` column references and arithmetic. Literal `Expr`s
/// route through [`Sql::push_param_typed`] so cast hinting (PG
/// `::TEXT` on NULL) still fires. `Column` writes a quoted ident;
/// `BinOp` emits `(<left> <op> <right>)` with both sides recursed.
fn write_expr(
    b: &mut Sql<'_>,
    expr: &crate::core::Expr,
    cast: Option<&'static str>,
) -> Result<(), SqlError> {
    use crate::core::{BinOp as BO, Expr};
    match expr {
        Expr::Literal(v) => {
            b.push_param_typed(v.clone(), cast);
            Ok(())
        }
        Expr::Column(name) => {
            b.write_ident(name);
            Ok(())
        }
        Expr::BinOp { left, op, right } => {
            // SQLite doesn't have a bitwise XOR operator; surface a
            // clear error rather than emitting silently-wrong SQL.
            if matches!(op, BO::BitXor) && b.d.name() == "sqlite" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "BitXor",
                    dialect: b.d.name(),
                });
            }
            b.sql.push('(');
            // Nested casts only apply at the literal leaf — clear here
            // so an outer NULL cast doesn't bleed into the operand.
            write_expr(b, left, None)?;
            b.sql.push(' ');
            b.sql.push_str(match op {
                BO::Add => "+",
                BO::Sub => "-",
                BO::Mul => "*",
                BO::Div => "/",
                BO::Mod => "%",
                BO::BitAnd => "&",
                BO::BitOr => "|",
                // PG spells XOR `#`; MySQL uses `^`. SQLite already
                // bounced above.
                BO::BitXor => {
                    if b.d.name() == "postgres" {
                        "#"
                    } else {
                        "^"
                    }
                }
                BO::BitShl => "<<",
                BO::BitShr => ">>",
            });
            b.sql.push(' ');
            write_expr(b, right, None)?;
            b.sql.push(')');
            Ok(())
        }
        Expr::Function { kind, args } => write_function(b, *kind, args),
    }
}

/// Emit a scalar function call. Most variants are straight `FN(args…)`
/// across all three dialects; the divergent ones (`Concat` on SQLite,
/// `Greatest`/`Least` on SQLite, `Substr` PG `FROM…FOR…` form) get
/// special-cased.
#[allow(clippy::too_many_lines)] // Per-fn arms are inherently linear.
fn write_function(
    b: &mut Sql<'_>,
    kind: crate::core::ScalarFn,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    use crate::core::ScalarFn as F;
    match kind {
        // -------- text: simple FN(arg) — unary, arity-checked --------
        F::Lower => write_call_unary(b, "LOWER", args),
        F::Upper => write_call_unary(b, "UPPER", args),
        F::Length => write_call_unary(b, "LENGTH", args),
        F::Trim => write_call_unary(b, "TRIM", args),
        F::LTrim => write_call_unary(b, "LTRIM", args),
        F::RTrim => write_call_unary(b, "RTRIM", args),

        // -------- text: 3-ary FN(s, from, to) --------
        F::Replace => {
            if args.len() != 3 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "REPLACE",
                    expected: "3",
                    got: args.len(),
                });
            }
            write_call(b, "REPLACE", args)
        }

        // -------- CONCAT: PG/MySQL native, SQLite `||` --------
        F::Concat => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "CONCAT",
                    expected: ">= 1",
                    got: 0,
                });
            }
            if b.d.name() == "sqlite" {
                // `||` chain. Parenthesize so precedence is unambiguous
                // when wrapped in another expression.
                b.sql.push('(');
                let mut first = true;
                for a in args {
                    if !first {
                        b.sql.push_str(" || ");
                    }
                    first = false;
                    write_expr(b, a, None)?;
                }
                b.sql.push(')');
                Ok(())
            } else {
                write_call(b, "CONCAT", args)
            }
        }

        // -------- SUBSTR: PG uses `FROM…FOR…`, MySQL/SQLite use commas --------
        F::Substr => {
            if args.len() != 3 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "SUBSTRING",
                    expected: "3",
                    got: args.len(),
                });
            }
            if b.d.name() == "postgres" {
                b.sql.push_str("SUBSTRING(");
                write_expr(b, &args[0], None)?;
                b.sql.push_str(" FROM ");
                write_expr(b, &args[1], None)?;
                b.sql.push_str(" FOR ");
                write_expr(b, &args[2], None)?;
                b.sql.push(')');
                Ok(())
            } else {
                // MySQL spells it SUBSTRING; SQLite spells it substr.
                // Both accept the comma form.
                let name = if b.d.name() == "mysql" {
                    "SUBSTRING"
                } else {
                    "SUBSTR"
                };
                write_call(b, name, args)
            }
        }

        // -------- math: simple unary, arity-checked --------
        F::Abs => write_call_unary(b, "ABS", args),
        F::Floor => write_call_unary(b, "FLOOR", args),
        F::Ceil => {
            // MySQL accepts both `CEIL` and `CEILING`; PG / SQLite use
            // `CEIL` (SQLite 3.35+). Emit `CEIL` everywhere for the
            // narrowest portable token.
            write_call_unary(b, "CEIL", args)
        }
        F::Round => {
            // 1- or 2-ary. The shape is identical across PG / MySQL /
            // SQLite at the SQL surface; precision-arg type semantics
            // diverge (PG `numeric` only), documented at the builder.
            if args.is_empty() || args.len() > 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "ROUND",
                    expected: "1 or 2",
                    got: args.len(),
                });
            }
            write_call(b, "ROUND", args)
        }

        // -------- comparison / NULL --------
        F::Coalesce => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "COALESCE",
                    expected: ">= 1",
                    got: 0,
                });
            }
            write_call(b, "COALESCE", args)
        }
        F::Greatest => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "GREATEST",
                    expected: ">= 1",
                    got: 0,
                });
            }
            // SQLite has no GREATEST keyword. Its scalar `MAX(a, b, …)`
            // form requires 2+ args; with 1 arg SQLite parses `MAX(x)` as
            // the AGGREGATE form, which is a misuse-of-aggregate error
            // inside `UPDATE SET` and the wrong semantic in `WHERE`.
            // Surface a clear error rather than emit silently-wrong SQL.
            if b.d.name() == "sqlite" && args.len() == 1 {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "GREATEST with 1 argument (SQLite collides with the aggregate MAX)",
                    dialect: "sqlite",
                });
            }
            let name = if b.d.name() == "sqlite" {
                "MAX"
            } else {
                "GREATEST"
            };
            write_call(b, name, args)
        }
        F::Least => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "LEAST",
                    expected: ">= 1",
                    got: 0,
                });
            }
            // See `Greatest` above for the SQLite 1-arg rationale.
            if b.d.name() == "sqlite" && args.len() == 1 {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "LEAST with 1 argument (SQLite collides with the aggregate MIN)",
                    dialect: "sqlite",
                });
            }
            let name = if b.d.name() == "sqlite" {
                "MIN"
            } else {
                "LEAST"
            };
            write_call(b, name, args)
        }
        F::NullIf => {
            if args.len() != 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "NULLIF",
                    expected: "2",
                    got: args.len(),
                });
            }
            write_call(b, "NULLIF", args)
        }
    }
}

/// Standard `NAME(arg, arg, …)` emit. Used by every function variant
/// whose dialect emission is identical across PG / MySQL / SQLite.
fn write_call(b: &mut Sql<'_>, name: &str, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    b.sql.push_str(name);
    b.sql.push('(');
    let mut first = true;
    for a in args {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        write_expr(b, a, None)?;
    }
    b.sql.push(')');
    Ok(())
}

/// `write_call` with an arity-1 assertion. Used for unary functions
/// (LOWER, UPPER, LENGTH, TRIM, LTRIM, RTRIM, ABS, CEIL, FLOOR) so a
/// hand-rolled `Expr::Function { args: vec![] }` or `vec![a, b]` fails
/// at emit-time with a clear error rather than reaching the database
/// with malformed SQL like `LOWER()` or `LENGTH(a, b)`. The public
/// builder API is type-locked to a single arg, so this only fires for
/// callers that construct the IR directly.
fn write_call_unary(
    b: &mut Sql<'_>,
    name: &'static str,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: name,
            expected: "1",
            got: args.len(),
        });
    }
    write_call(b, name, args)
}

// ====================================================================
// DELETE
// ====================================================================

pub(super) fn write_delete(b: &mut Sql<'_>, query: &DeleteQuery) -> Result<(), SqlError> {
    b.sql.push_str("DELETE FROM ");
    b.write_ident(query.model.table);
    write_where(b, &query.where_clause, Some(query.model))?;
    Ok(())
}

// ====================================================================
// BULK UPDATE — Postgres-only `UPDATE … FROM (VALUES …)` shape.
// MySQL would need a different translation (CASE WHEN, or
// CREATE TEMP TABLE + JOIN); leaving that for batch4. For now the
// MySql dialect routes bulk_update through a clear "not supported"
// error in its own compile_bulk_update.
// ====================================================================

pub(super) fn write_bulk_update_pg(
    b: &mut Sql<'_>,
    query: &BulkUpdateQuery,
) -> Result<(), SqlError> {
    if query.rows.is_empty() {
        return Err(SqlError::EmptyBulkInsert);
    }
    if query.update_columns.is_empty() {
        return Err(SqlError::EmptyUpdateSet);
    }
    let pk_field = query
        .model
        .primary_key()
        .ok_or(SqlError::MissingPrimaryKey)?;

    b.sql.push_str("UPDATE ");
    b.write_ident(query.model.table);
    b.sql.push_str(" SET ");
    let mut first = true;
    for col in &query.update_columns {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        b.write_ident(col);
        b.sql.push_str(" = __data.");
        b.write_ident(col);
    }
    b.sql.push_str(" FROM (VALUES ");
    let mut first_row = true;
    for row in &query.rows {
        if !first_row {
            b.sql.push_str(", ");
        }
        first_row = false;
        b.sql.push('(');
        for (i, val) in row.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.push_param(val.clone());
        }
        b.sql.push(')');
    }
    b.sql.push_str(") AS __data(");
    b.write_ident(pk_field.column);
    for col in &query.update_columns {
        b.sql.push_str(", ");
        b.write_ident(col);
    }
    b.sql.push_str(") WHERE ");
    b.write_ident(query.model.table);
    b.sql.push('.');
    b.write_ident(pk_field.column);
    b.sql.push_str(" = __data.");
    b.write_ident(pk_field.column);
    Ok(())
}

// ====================================================================
// WHERE / Filters
// ====================================================================

pub(super) fn write_where(
    b: &mut Sql<'_>,
    where_clause: &WhereExpr,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    if where_clause.is_empty() {
        return Ok(());
    }
    b.sql.push_str(" WHERE ");
    write_where_expr(b, where_clause, None, model)
}

pub(super) fn write_where_with_search(
    b: &mut Sql<'_>,
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let has_search = search.is_some_and(|s| !s.columns.is_empty() && !s.query.is_empty());
    let has_where = !where_clause.is_empty();
    if !has_where && !has_search {
        return Ok(());
    }
    b.sql.push_str(" WHERE ");
    if has_where {
        write_where_expr(b, where_clause, qualify_with, model)?;
    }
    if has_search {
        let s = search.expect("checked above");
        if has_where {
            b.sql.push_str(" AND ");
        }
        // Search routes through the dialect's `write_ilike` so each
        // backend emits the case-insensitive LIKE shape it actually
        // supports — Postgres native `ILIKE`, MySQL/SQLite the
        // `LOWER(col) LIKE LOWER(?)` fallback. v0.37 fix: previous
        // code wrote the literal `ILIKE` token whenever
        // `supports_op(Op::ILike)` returned true, but SQLite says
        // true (because it can *lower* via `write_ilike`) yet has no
        // native `ILIKE` keyword, so the emitted SQL failed at parse.
        b.params.push(SqlValue::String(format!("%{}%", s.query)));
        let placeholder = b.d.placeholder(b.params.len());
        b.sql.push('(');
        for (i, col) in s.columns.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(" OR ");
            }
            // Build the qualified column identifier the same way the
            // rest of the writer does, then hand it to `write_ilike`.
            let mut qualified = String::new();
            if let Some(table) = qualify_with {
                qualified.push_str(&b.d.quote_ident(table));
                qualified.push('.');
            }
            qualified.push_str(&b.d.quote_ident(col));
            b.d.write_ilike(&mut b.sql, &qualified, &placeholder, false);
        }
        b.sql.push(')');
    }
    Ok(())
}

pub(super) fn write_where_expr(
    b: &mut Sql<'_>,
    expr: &WhereExpr,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => write_filter(b, filter, qualify_with, model),
        WhereExpr::ColumnCompare(cf) => write_column_compare(b, cf, qualify_with, model),
        WhereExpr::And(items) => write_joined(b, items, " AND ", qualify_with, model),
        WhereExpr::Or(items) => {
            if items.is_empty() {
                return Err(SqlError::EmptyOrBranch);
            }
            write_joined(b, items, " OR ", qualify_with, model)
        }
        WhereExpr::Not(child) => {
            b.sql.push_str("NOT (");
            write_where_expr(b, child, qualify_with, model)?;
            b.sql.push(')');
            Ok(())
        }
    }
}

/// Render `<col> <op> <rhs-expr>` for a [`crate::core::ColumnFilter`].
/// Only the binary-comparison `Op` variants are valid here — anything
/// else (`In`, `Between`, `IsNull`, JSON ops, etc.) is a builder error
/// and surfaces as [`SqlError::OpNotSupportedInDialect`] so the test
/// suite catches it.
fn write_column_compare(
    b: &mut Sql<'_>,
    cf: &crate::core::ColumnFilter,
    qualify_with: Option<&str>,
    _model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let qualified = render_qualified_col(b.d, qualify_with, cf.column);
    b.sql.push_str(&qualified);
    let op_str = match cf.op {
        crate::core::Op::Eq => " = ",
        crate::core::Op::Ne => " <> ",
        crate::core::Op::Lt => " < ",
        crate::core::Op::Lte => " <= ",
        crate::core::Op::Gt => " > ",
        crate::core::Op::Gte => " >= ",
        // Other ops don't fit a `col <op> col` shape; reject loudly.
        _ => {
            return Err(SqlError::OpNotSupportedInDialect {
                op: "non-binary comparison in ColumnCompare",
                dialect: b.d.name(),
            });
        }
    };
    b.sql.push_str(op_str);
    write_expr(b, &cf.rhs, None)?;
    Ok(())
}

fn write_joined(
    b: &mut Sql<'_>,
    items: &[WhereExpr],
    sep: &str,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let mut first = true;
    for child in items {
        if !first {
            b.sql.push_str(sep);
        }
        first = false;
        write_child(b, child, qualify_with, model)?;
    }
    Ok(())
}

fn write_child(
    b: &mut Sql<'_>,
    expr: &WhereExpr,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => write_filter(b, filter, qualify_with, model),
        WhereExpr::ColumnCompare(cf) => write_column_compare(b, cf, qualify_with, model),
        WhereExpr::And(_) | WhereExpr::Or(_) | WhereExpr::Not(_) => {
            b.sql.push('(');
            write_where_expr(b, expr, qualify_with, model)?;
            b.sql.push(')');
            Ok(())
        }
    }
}

#[allow(clippy::too_many_lines)] // The op match arms inflate this; splitting per op group hurts readability.
fn write_filter(
    b: &mut Sql<'_>,
    filter: &Filter,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let qualified_col = render_qualified_col(b.d, qualify_with, filter.column);
    let cast = model.and_then(|m| null_cast_for(b.d, m, filter.column));

    match filter.op {
        Op::Eq => simple_op(b, &qualified_col, " = ", filter.value.clone(), cast),
        Op::Ne => simple_op(b, &qualified_col, " <> ", filter.value.clone(), cast),
        Op::Lt => simple_op(b, &qualified_col, " < ", filter.value.clone(), cast),
        Op::Lte => simple_op(b, &qualified_col, " <= ", filter.value.clone(), cast),
        Op::Gt => simple_op(b, &qualified_col, " > ", filter.value.clone(), cast),
        Op::Gte => simple_op(b, &qualified_col, " >= ", filter.value.clone(), cast),
        Op::Like => simple_op(b, &qualified_col, " LIKE ", filter.value.clone(), cast),
        Op::NotLike => simple_op(b, &qualified_col, " NOT LIKE ", filter.value.clone(), cast),
        Op::ILike | Op::NotILike => {
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_ilike(
                &mut b.sql,
                &qualified_col,
                &p,
                matches!(filter.op, Op::NotILike),
            );
        }
        Op::In | Op::NotIn => {
            let SqlValue::List(elements) = &filter.value else {
                return Err(SqlError::InRequiresList);
            };
            if elements.is_empty() {
                return Err(SqlError::EmptyInList);
            }
            b.sql.push_str(&qualified_col);
            b.sql.push_str(if matches!(filter.op, Op::In) {
                " IN ("
            } else {
                " NOT IN ("
            });
            let mut first = true;
            for elem in elements {
                if !first {
                    b.sql.push_str(", ");
                }
                first = false;
                b.push_param_typed(elem.clone(), cast);
            }
            b.sql.push(')');
        }
        Op::Between => {
            let SqlValue::List(bounds) = &filter.value else {
                return Err(SqlError::BetweenRequiresTwoElementList);
            };
            if bounds.len() != 2 {
                return Err(SqlError::BetweenRequiresTwoElementList);
            }
            b.sql.push_str(&qualified_col);
            b.sql.push_str(" BETWEEN ");
            b.push_param_typed(bounds[0].clone(), cast);
            b.sql.push_str(" AND ");
            b.push_param_typed(bounds[1].clone(), cast);
        }
        Op::IsNull => {
            let SqlValue::Bool(is_null) = filter.value else {
                return Err(SqlError::IsNullRequiresBool);
            };
            b.sql.push_str(&qualified_col);
            b.sql
                .push_str(if is_null { " IS NULL" } else { " IS NOT NULL" });
        }
        Op::IsDistinctFrom | Op::IsNotDistinctFrom => {
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_null_safe_eq(
                &mut b.sql,
                &qualified_col,
                &p,
                matches!(filter.op, Op::IsDistinctFrom),
            );
        }
        Op::JsonContains => {
            require_op(b.d, filter.op)?;
            let SqlValue::Json(_) = &filter.value else {
                return Err(SqlError::JsonOpRequiresJson);
            };
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_json_contains(&mut b.sql, &qualified_col, &p);
        }
        Op::JsonContainedBy => {
            require_op(b.d, filter.op)?;
            let SqlValue::Json(_) = &filter.value else {
                return Err(SqlError::JsonOpRequiresJson);
            };
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_json_contained_by(&mut b.sql, &qualified_col, &p);
        }
        Op::JsonHasKey => {
            require_op(b.d, filter.op)?;
            let SqlValue::String(_) = &filter.value else {
                return Err(SqlError::JsonKeyRequiresString);
            };
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_json_has_key(&mut b.sql, &qualified_col, &p);
        }
        Op::JsonHasAnyKey | Op::JsonHasAllKeys => {
            require_op(b.d, filter.op)?;
            let SqlValue::List(keys) = &filter.value else {
                return Err(SqlError::JsonKeysRequiresList);
            };
            // Bind each key as its own param, collect the placeholder
            // strings, then ask the dialect to compose the predicate.
            // PG produces `col ?| ARRAY[$1,$2]` / `col ?& ARRAY[$1,$2]`;
            // MySQL produces `JSON_CONTAINS_PATH(col, 'one'|'all',
            // CONCAT('$.', ?), CONCAT('$.', ?))`.
            let placeholders = bind_param_list(b, keys);
            if matches!(filter.op, Op::JsonHasAnyKey) {
                b.d.write_json_has_any_keys(&mut b.sql, &qualified_col, &placeholders);
            } else {
                b.d.write_json_has_all_keys(&mut b.sql, &qualified_col, &placeholders);
            }
        }
    }
    Ok(())
}

fn simple_op(
    b: &mut Sql<'_>,
    qualified_col: &str,
    kw: &str,
    value: SqlValue,
    cast: Option<&'static str>,
) {
    b.sql.push_str(qualified_col);
    b.sql.push_str(kw);
    b.push_param_typed(value, cast);
}

/// Render `[<table>.]<col>` using the dialect's quoting rules.
/// Allocated up front so `write_filter`'s op handlers can either emit
/// it directly or wrap it (e.g. `LOWER(<col>) LIKE …`) without having
/// to backtrack writes already on the buffer.
fn render_qualified_col(d: &dyn Dialect, qualify_with: Option<&str>, column: &str) -> String {
    let mut s = String::new();
    if let Some(table) = qualify_with {
        s.push_str(&d.quote_ident(table));
        s.push('.');
    }
    s.push_str(&d.quote_ident(column));
    s
}

/// Bind each value in `values` as a param without writing anything to
/// `b.sql`; return the placeholder strings (`$1`, `$2`, … on Postgres;
/// `?`, `?`, … on MySQL) so a per-dialect predicate writer can compose
/// them into the final fragment.
fn bind_param_list(b: &mut Sql<'_>, values: &[SqlValue]) -> Vec<String> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        b.params.push(v.clone());
        out.push(b.d.placeholder(b.params.len()));
    }
    out
}

fn require_op(d: &dyn Dialect, op: Op) -> Result<(), SqlError> {
    if d.supports_op(op) {
        Ok(())
    } else {
        Err(SqlError::OperatorNotSupportedInDialect {
            op: op_label(op),
            dialect: d.name(),
        })
    }
}

fn op_label(op: Op) -> &'static str {
    match op {
        Op::Eq => "=",
        Op::Ne => "<>",
        Op::Lt => "<",
        Op::Lte => "<=",
        Op::Gt => ">",
        Op::Gte => ">=",
        Op::In => "IN",
        Op::NotIn => "NOT IN",
        Op::Like => "LIKE",
        Op::NotLike => "NOT LIKE",
        Op::ILike => "ILIKE",
        Op::NotILike => "NOT ILIKE",
        Op::Between => "BETWEEN",
        Op::IsNull => "IS NULL",
        Op::IsDistinctFrom => "IS DISTINCT FROM",
        Op::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
        Op::JsonContains => "@>",
        Op::JsonContainedBy => "<@",
        Op::JsonHasKey => "? (json)",
        Op::JsonHasAnyKey => "?| (json)",
        Op::JsonHasAllKeys => "?& (json)",
    }
}

// ====================================================================
// ORDER BY / LIMIT / OFFSET
// ====================================================================

fn write_order_limit_offset(
    b: &mut Sql<'_>,
    order_by: &[OrderClause],
    limit: Option<i64>,
    offset: Option<i64>,
    qualify_with: Option<&str>,
) {
    if !order_by.is_empty() {
        b.sql.push_str(" ORDER BY ");
        for (i, clause) in order_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            if let Some(table) = qualify_with {
                b.write_ident(table);
                b.sql.push('.');
            }
            b.write_ident(clause.column);
            if clause.desc {
                b.sql.push_str(" DESC");
            }
        }
    }
    if let Some(n) = limit {
        let _ = write!(b.sql, " LIMIT {n}");
    }
    if let Some(n) = offset {
        let _ = write!(b.sql, " OFFSET {n}");
    }
}

// ====================================================================
// RETURNING
// ====================================================================

fn write_returning(b: &mut Sql<'_>, returning: &[&'static str]) -> Result<(), SqlError> {
    if returning.is_empty() {
        return Ok(());
    }
    if !b.d.supports_returning() {
        // Caller is expected to fall back to LAST_INSERT_ID() / similar
        // — but that decision belongs at the executor layer (where we
        // know whether the model has an Auto<T> PK). Surface a clear
        // error here so the executor can detect + handle it instead of
        // silently producing SQL the backend rejects.
        return Err(SqlError::OperatorNotSupportedInDialect {
            op: "RETURNING",
            dialect: b.d.name(),
        });
    }
    b.sql.push_str(" RETURNING ");
    let mut first = true;
    for col in returning {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        b.write_ident(col);
    }
    Ok(())
}

// ====================================================================
// Helper exposed to the legacy `compile_where_order_tail` shim
// (annotate_count_children calls this directly).
// ====================================================================

#[allow(unused)]
#[allow(clippy::too_many_arguments)] // 8 args mirrors the existing public shim signature; refactoring the call-sites is a v0.24 cleanup.
pub(crate) fn compile_where_order_tail(
    d: &dyn Dialect,
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    order_by: &[OrderClause],
    limit: Option<i64>,
    offset: Option<i64>,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<CompiledStatement, SqlError> {
    let mut b = Sql::new(d);
    write_where_with_search(&mut b, where_clause, search, qualify_with, model)?;
    write_order_limit_offset(&mut b, order_by, limit, offset, qualify_with);
    Ok(b.finish())
}
