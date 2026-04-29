//! Postgres dialect: double-quoted identifiers, `$1`-style placeholders.

use std::fmt::Write as _;

use rustango_core::{
    BulkInsertQuery, CountQuery, DeleteQuery, FieldType, Filter, InsertQuery, ModelSchema, Op,
    SearchClause, SelectQuery, SqlValue, UpdateQuery,
};

use crate::{CompiledStatement, Dialect, SqlError};

/// The Postgres dialect.
///
/// Stateless; construct with `Postgres` and call [`Dialect::compile_select`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Postgres;

impl Dialect for Postgres {
    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError> {
        let mut sql = String::new();
        let mut params: Vec<SqlValue> = Vec::new();
        let qualify = !query.joins.is_empty();

        sql.push_str("SELECT ");
        let mut first_col = true;
        // Main table columns. Qualified when joins are present so column
        // names don't collide with joined ones.
        for field in query.model.scalar_fields() {
            if !first_col {
                sql.push_str(", ");
            }
            first_col = false;
            if qualify {
                write_ident(&mut sql, query.model.table);
                sql.push('.');
            }
            write_ident(&mut sql, field.column);
        }
        // Joined columns, aliased as `<alias>__<col>`.
        for join in &query.joins {
            for col in &join.project {
                sql.push_str(", ");
                write_ident(&mut sql, join.alias);
                sql.push('.');
                write_ident(&mut sql, col);
                sql.push_str(" AS ");
                write_ident(&mut sql, &format!("{}__{}", join.alias, col));
            }
        }

        sql.push_str(" FROM ");
        write_ident(&mut sql, query.model.table);

        for join in &query.joins {
            sql.push_str(" LEFT JOIN ");
            write_ident(&mut sql, join.target.table);
            sql.push_str(" AS ");
            write_ident(&mut sql, join.alias);
            sql.push_str(" ON ");
            write_ident(&mut sql, query.model.table);
            sql.push('.');
            write_ident(&mut sql, join.on_local);
            sql.push_str(" = ");
            write_ident(&mut sql, join.alias);
            sql.push('.');
            write_ident(&mut sql, join.on_remote);
        }

        write_where_with_search_qualified(
            &mut sql,
            &mut params,
            &query.filters,
            query.search.as_ref(),
            qualify.then_some(query.model.table),
            Some(query.model),
        )?;

        if let Some(limit) = query.limit {
            let _ = write!(sql, " LIMIT {limit}");
        }
        if let Some(offset) = query.offset {
            let _ = write!(sql, " OFFSET {offset}");
        }

        Ok(CompiledStatement { sql, params })
    }

    fn compile_count(&self, query: &CountQuery) -> Result<CompiledStatement, SqlError> {
        let mut sql = String::from("SELECT COUNT(*) FROM ");
        let mut params: Vec<SqlValue> = Vec::new();
        write_ident(&mut sql, query.model.table);
        write_where(&mut sql, &mut params, &query.filters, Some(query.model))?;
        Ok(CompiledStatement { sql, params })
    }

    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError> {
        // `columns.is_empty()` is OK when every column is being filled
        // by a server-side default — typically an `Auto<T>`-only model.
        // In that case we emit `INSERT INTO t DEFAULT VALUES`. Without
        // `RETURNING`, a fully-empty insert is a footgun, so we still
        // reject it.
        if query.columns.is_empty() && query.returning.is_empty() {
            return Err(SqlError::EmptyInsert);
        }
        if query.columns.len() != query.values.len() {
            return Err(SqlError::InsertShapeMismatch {
                columns: query.columns.len(),
                values: query.values.len(),
            });
        }

        let mut sql = String::new();
        let mut params: Vec<SqlValue> = Vec::with_capacity(query.values.len());

        sql.push_str("INSERT INTO ");
        write_ident(&mut sql, query.model.table);

        if query.columns.is_empty() {
            sql.push_str(" DEFAULT VALUES");
        } else {
            sql.push_str(" (");
            let mut first = true;
            for col in &query.columns {
                if !first {
                    sql.push_str(", ");
                }
                first = false;
                write_ident(&mut sql, col);
            }
            sql.push_str(") VALUES (");
            let mut first = true;
            for (col, value) in query.columns.iter().zip(&query.values) {
                if !first {
                    sql.push_str(", ");
                }
                first = false;
                let cast = pg_null_cast_for(query.model, col);
                push_param_typed(&mut sql, &mut params, value.clone(), cast);
            }
            sql.push(')');
        }

        if !query.returning.is_empty() {
            sql.push_str(" RETURNING ");
            let mut first = true;
            for col in &query.returning {
                if !first {
                    sql.push_str(", ");
                }
                first = false;
                write_ident(&mut sql, col);
            }
        }

        Ok(CompiledStatement { sql, params })
    }

    fn compile_bulk_insert(
        &self,
        query: &BulkInsertQuery,
    ) -> Result<CompiledStatement, SqlError> {
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

        let mut sql = String::new();
        let mut params: Vec<SqlValue> = Vec::with_capacity(query.columns.len() * query.rows.len());

        sql.push_str("INSERT INTO ");
        write_ident(&mut sql, query.model.table);

        if query.columns.is_empty() {
            // All-Auto-Unset bulk: every row is just `DEFAULT VALUES`.
            // Postgres requires one such clause per row, separated by
            // commas — so emit `INSERT INTO t SELECT … UNION ALL …`?
            // Simpler: VALUES with no parens-group is illegal. We
            // can't construct a no-column multi-row insert with
            // `DEFAULT VALUES`. Emit one `INSERT … DEFAULT VALUES`
            // per row would mean N round-trips; defeats the purpose.
            // Instead, emit `INSERT INTO t (pk) VALUES (DEFAULT), …`
            // referencing the first returning column as the
            // "placeholder" — Postgres treats DEFAULT as the
            // sequence-driven value the same way.
            let pk = query.returning.first().copied().ok_or(SqlError::EmptyInsert)?;
            sql.push_str(" (");
            write_ident(&mut sql, pk);
            sql.push_str(") VALUES ");
            let mut first_row = true;
            for _ in &query.rows {
                if !first_row {
                    sql.push_str(", ");
                }
                first_row = false;
                sql.push_str("(DEFAULT)");
            }
        } else {
            sql.push_str(" (");
            let mut first = true;
            for col in &query.columns {
                if !first {
                    sql.push_str(", ");
                }
                first = false;
                write_ident(&mut sql, col);
            }
            sql.push_str(") VALUES ");

            let mut first_row = true;
            for row in &query.rows {
                if !first_row {
                    sql.push_str(", ");
                }
                first_row = false;
                sql.push('(');
                let mut first_v = true;
                for (col, value) in query.columns.iter().zip(row) {
                    if !first_v {
                        sql.push_str(", ");
                    }
                    first_v = false;
                    let cast = pg_null_cast_for(query.model, col);
                    push_param_typed(&mut sql, &mut params, value.clone(), cast);
                }
                sql.push(')');
            }
        }

        if !query.returning.is_empty() {
            sql.push_str(" RETURNING ");
            let mut first = true;
            for col in &query.returning {
                if !first {
                    sql.push_str(", ");
                }
                first = false;
                write_ident(&mut sql, col);
            }
        }

        Ok(CompiledStatement { sql, params })
    }

    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError> {
        if query.set.is_empty() {
            return Err(SqlError::EmptyUpdateSet);
        }

        let mut sql = String::from("UPDATE ");
        let mut params: Vec<SqlValue> = Vec::new();
        write_ident(&mut sql, query.model.table);
        sql.push_str(" SET ");

        let mut first = true;
        for assignment in &query.set {
            if !first {
                sql.push_str(", ");
            }
            first = false;
            write_ident(&mut sql, assignment.column);
            sql.push_str(" = ");
            let cast = pg_null_cast_for(query.model, assignment.column);
            push_param_typed(&mut sql, &mut params, assignment.value.clone(), cast);
        }

        write_where(&mut sql, &mut params, &query.filters, Some(query.model))?;

        Ok(CompiledStatement { sql, params })
    }

    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        let mut sql = String::from("DELETE FROM ");
        let mut params: Vec<SqlValue> = Vec::new();
        write_ident(&mut sql, query.model.table);

        write_where(&mut sql, &mut params, &query.filters, Some(query.model))?;

        Ok(CompiledStatement { sql, params })
    }
}

fn write_where(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    filters: &[Filter],
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    if filters.is_empty() {
        return Ok(());
    }
    sql.push_str(" WHERE ");
    let mut first = true;
    for filter in filters {
        if !first {
            sql.push_str(" AND ");
        }
        first = false;
        write_filter(sql, params, filter, model)?;
    }
    Ok(())
}

/// Append a parenthesized `(col1 ILIKE $N OR col2 ILIKE $N …)` clause
/// when `search` is `Some` with non-empty `columns` and a non-empty
/// `query`. The same parameter position is reused across all OR-ed
/// columns. When `qualify_with` is `Some(table)`, every column reference
/// is prefixed with `"<table>"."…"` so the WHERE survives joins.
/// column references with `"<table>"."…"` when `qualify_with` is `Some`.
/// Used by `compile_select` when joins are active so the WHERE clause
/// disambiguates main-table columns from joined ones.
fn write_where_with_search_qualified(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    filters: &[Filter],
    search: Option<&SearchClause>,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let has_search = search.is_some_and(|s| !s.columns.is_empty() && !s.query.is_empty());
    if filters.is_empty() && !has_search {
        return Ok(());
    }
    sql.push_str(" WHERE ");
    let mut first = true;
    for filter in filters {
        if !first {
            sql.push_str(" AND ");
        }
        first = false;
        write_filter_qualified(sql, params, filter, qualify_with, model)?;
    }
    if has_search {
        let s = search.expect("checked above");
        if !first {
            sql.push_str(" AND ");
        }
        params.push(SqlValue::String(format!("%{}%", s.query)));
        let placeholder = params.len();
        sql.push('(');
        for (i, col) in s.columns.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            if let Some(table) = qualify_with {
                write_ident(sql, table);
                sql.push('.');
            }
            write_ident(sql, col);
            let _ = write!(sql, " ILIKE ${placeholder}");
        }
        sql.push(')');
    }
    Ok(())
}

fn write_filter(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    filter: &Filter,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    write_filter_qualified(sql, params, filter, None, model)
}

fn write_filter_qualified(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    filter: &Filter,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    if let Some(table) = qualify_with {
        write_ident(sql, table);
        sql.push('.');
    }
    write_ident(sql, filter.column);

    let cast = model.and_then(|m| pg_null_cast_for(m, filter.column));

    match filter.op {
        Op::Eq => {
            sql.push_str(" = ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::Ne => {
            sql.push_str(" <> ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::Lt => {
            sql.push_str(" < ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::Lte => {
            sql.push_str(" <= ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::Gt => {
            sql.push_str(" > ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::Gte => {
            sql.push_str(" >= ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::Like => {
            sql.push_str(" LIKE ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::In => {
            let SqlValue::List(elements) = &filter.value else {
                return Err(SqlError::InRequiresList);
            };
            if elements.is_empty() {
                return Err(SqlError::EmptyInList);
            }
            sql.push_str(" IN (");
            let mut first = true;
            for elem in elements {
                if !first {
                    sql.push_str(", ");
                }
                first = false;
                push_param_typed(sql, params, elem.clone(), cast);
            }
            sql.push(')');
        }
        Op::IsNull => {
            let SqlValue::Bool(is_null) = filter.value else {
                return Err(SqlError::IsNullRequiresBool);
            };
            sql.push_str(if is_null { " IS NULL" } else { " IS NOT NULL" });
        }
    }
    Ok(())
}

/// Emit `$N` for a non-null value, or `$N::PGTYPE` when the value is
/// `SqlValue::Null` and a column hint is supplied. Postgres rejects an
/// untyped/text NULL bound against an integer column with
/// `column "x" is of type integer but expression is of type text`; the
/// cast tells Postgres exactly what NULL we mean. Non-null values go
/// through unchanged (sqlx's binding is already typed correctly).
fn push_param_typed(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    value: SqlValue,
    pg_type: Option<&'static str>,
) {
    let is_null = matches!(value, SqlValue::Null);
    params.push(value);
    let _ = write!(sql, "${}", params.len());
    if is_null {
        if let Some(ty) = pg_type {
            let _ = write!(sql, "::{ty}");
        }
    }
}

/// Postgres type a NULL parameter should be cast to when the column
/// is known. Coarser than the full DDL `sql_type` (no `VARCHAR(N)`
/// length, no CHECK) — for the cast we only need the *family*.
fn pg_null_cast_for(model: &ModelSchema, column: &str) -> Option<&'static str> {
    let field = model.field_by_column(column)?;
    Some(match field.ty {
        FieldType::I32 => "INTEGER",
        FieldType::I64 => "BIGINT",
        FieldType::F32 => "REAL",
        FieldType::F64 => "DOUBLE PRECISION",
        FieldType::Bool => "BOOLEAN",
        FieldType::String => "TEXT",
        FieldType::DateTime => "TIMESTAMPTZ",
        FieldType::Date => "DATE",
        FieldType::Uuid => "UUID",
        FieldType::Json => "JSONB",
    })
}

fn write_ident(sql: &mut String, name: &str) {
    sql.push('"');
    for ch in name.chars() {
        if ch == '"' {
            sql.push_str("\"\"");
        } else {
            sql.push(ch);
        }
    }
    sql.push('"');
}
