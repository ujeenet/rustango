//! Postgres dialect: double-quoted identifiers, `$1`-style placeholders.

use std::fmt::Write as _;

use rustango_core::{
    CountQuery, DeleteQuery, Filter, InsertQuery, Op, SearchClause, SelectQuery, SqlValue,
    UpdateQuery,
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

        sql.push_str("SELECT ");
        let mut first_col = true;
        for field in query.model.scalar_fields() {
            if !first_col {
                sql.push_str(", ");
            }
            first_col = false;
            write_ident(&mut sql, field.column);
        }

        sql.push_str(" FROM ");
        write_ident(&mut sql, query.model.table);

        write_where_with_search(&mut sql, &mut params, &query.filters, query.search.as_ref())?;

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
        write_where(&mut sql, &mut params, &query.filters)?;
        Ok(CompiledStatement { sql, params })
    }

    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError> {
        if query.columns.is_empty() {
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
        for value in &query.values {
            if !first {
                sql.push_str(", ");
            }
            first = false;
            push_param(&mut sql, &mut params, value.clone());
        }
        sql.push(')');

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
            push_param(&mut sql, &mut params, assignment.value.clone());
        }

        write_where(&mut sql, &mut params, &query.filters)?;

        Ok(CompiledStatement { sql, params })
    }

    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        let mut sql = String::from("DELETE FROM ");
        let mut params: Vec<SqlValue> = Vec::new();
        write_ident(&mut sql, query.model.table);

        write_where(&mut sql, &mut params, &query.filters)?;

        Ok(CompiledStatement { sql, params })
    }
}

fn write_where(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    filters: &[Filter],
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
        write_filter(sql, params, filter)?;
    }
    Ok(())
}

/// Like [`write_where`] but additionally appends a parenthesized
/// `(col1 ILIKE $N OR col2 ILIKE $N …)` clause when `search` is `Some`
/// with non-empty `columns` and a non-empty `query`. The same parameter
/// position is reused across all OR-ed columns.
fn write_where_with_search(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    filters: &[Filter],
    search: Option<&SearchClause>,
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
        write_filter(sql, params, filter)?;
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
) -> Result<(), SqlError> {
    write_ident(sql, filter.column);

    match filter.op {
        Op::Eq => {
            sql.push_str(" = ");
            push_param(sql, params, filter.value.clone());
        }
        Op::Ne => {
            sql.push_str(" <> ");
            push_param(sql, params, filter.value.clone());
        }
        Op::Lt => {
            sql.push_str(" < ");
            push_param(sql, params, filter.value.clone());
        }
        Op::Lte => {
            sql.push_str(" <= ");
            push_param(sql, params, filter.value.clone());
        }
        Op::Gt => {
            sql.push_str(" > ");
            push_param(sql, params, filter.value.clone());
        }
        Op::Gte => {
            sql.push_str(" >= ");
            push_param(sql, params, filter.value.clone());
        }
        Op::Like => {
            sql.push_str(" LIKE ");
            push_param(sql, params, filter.value.clone());
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
                push_param(sql, params, elem.clone());
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

fn push_param(sql: &mut String, params: &mut Vec<SqlValue>, value: SqlValue) {
    params.push(value);
    // Length post-push gives the 1-based placeholder index Postgres expects.
    let _ = write!(sql, "${}", params.len());
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
