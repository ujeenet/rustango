//! Postgres dialect: double-quoted identifiers, `$1`-style placeholders.

use std::fmt::Write as _;

use rustango_core::{Filter, Op, SelectQuery, SqlValue};

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

        if !query.filters.is_empty() {
            sql.push_str(" WHERE ");
            let mut first_filter = true;
            for filter in &query.filters {
                if !first_filter {
                    sql.push_str(" AND ");
                }
                first_filter = false;
                write_filter(&mut sql, &mut params, filter)?;
            }
        }

        Ok(CompiledStatement { sql, params })
    }
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
