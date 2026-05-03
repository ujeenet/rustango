//! Postgres dialect: double-quoted identifiers, `$1`-style placeholders.

use std::fmt::Write as _;

use crate::core::{
    AggregateExpr, AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery,
    DeleteQuery, FieldType, Filter, InsertQuery, ModelSchema, Op, OrderClause, SearchClause,
    SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};

use super::{CompiledStatement, Dialect, SqlError};

/// The Postgres dialect.
///
/// Stateless; construct with `Postgres` and call [`Dialect::compile_select`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Postgres;

/// `'static` reference to the singleton [`Postgres`] dialect, useful
/// where callers want a `&'static dyn Dialect` (e.g. [`crate::sql::Pool::dialect`]).
pub static DIALECT: &Postgres = &Postgres;

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    // Postgres uses ANSI-style double-quoted identifiers — same as the
    // trait default, no override needed for `quote_ident`.

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "SERIAL",
            _ => "BIGSERIAL",
        }
    }

    // Postgres has a native `BOOLEAN` type with `TRUE` / `FALSE`
    // literals — same as the trait default, no override.

    fn supports_concurrent_index(&self) -> bool {
        true
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn acquire_session_lock_sql(&self) -> Option<String> {
        Some(format!("SELECT pg_advisory_lock({})", self.placeholder(1)))
    }

    fn release_session_lock_sql(&self) -> Option<String> {
        Some(format!(
            "SELECT pg_advisory_unlock({})",
            self.placeholder(1)
        ))
    }

    fn acquire_xact_lock_sql(&self) -> Option<String> {
        Some(format!(
            "SELECT pg_advisory_xact_lock({})",
            self.placeholder(1)
        ))
    }

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
            &query.where_clause,
            query.search.as_ref(),
            qualify.then_some(query.model.table),
            Some(query.model),
        )?;

        // Slice 9.0b — `ORDER BY "col" [DESC]` per registered clause,
        // comma-separated. Emitted after WHERE / joins but before
        // LIMIT / OFFSET so the database can apply the ordering
        // before the slice is taken.
        if !query.order_by.is_empty() {
            sql.push_str(" ORDER BY ");
            for (i, clause) in query.order_by.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                if qualify {
                    write_ident(&mut sql, query.model.table);
                    sql.push('.');
                }
                write_ident(&mut sql, clause.column);
                if clause.desc {
                    sql.push_str(" DESC");
                }
            }
        }

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
        write_where(&mut sql, &mut params, &query.where_clause, Some(query.model))?;
        Ok(CompiledStatement { sql, params })
    }

    fn compile_aggregate(&self, query: &AggregateQuery) -> Result<CompiledStatement, SqlError> {
        let mut sql = String::from("SELECT ");
        let mut params: Vec<SqlValue> = Vec::new();

        // GROUP BY columns first, then aggregate expressions.
        for (i, col) in query.group_by.iter().enumerate() {
            if i > 0 { sql.push_str(", "); }
            write_ident(&mut sql, col);
        }
        for (i, (alias, expr)) in query.aggregates.iter().enumerate() {
            if !query.group_by.is_empty() || i > 0 { sql.push_str(", "); }
            match expr {
                AggregateExpr::Count(None) => sql.push_str("COUNT(*)"),
                AggregateExpr::Count(Some(col)) => {
                    sql.push_str("COUNT(");
                    write_ident(&mut sql, col);
                    sql.push(')');
                }
                AggregateExpr::Sum(col) => {
                    sql.push_str("SUM(");
                    write_ident(&mut sql, col);
                    sql.push(')');
                }
                AggregateExpr::Avg(col) => {
                    sql.push_str("AVG(");
                    write_ident(&mut sql, col);
                    sql.push(')');
                }
                AggregateExpr::Max(col) => {
                    sql.push_str("MAX(");
                    write_ident(&mut sql, col);
                    sql.push(')');
                }
                AggregateExpr::Min(col) => {
                    sql.push_str("MIN(");
                    write_ident(&mut sql, col);
                    sql.push(')');
                }
            }
            sql.push_str(" AS ");
            write_ident(&mut sql, alias);
        }

        sql.push_str(" FROM ");
        write_ident(&mut sql, query.model.table);
        write_where(&mut sql, &mut params, &query.where_clause, Some(query.model))?;

        if !query.group_by.is_empty() {
            sql.push_str(" GROUP BY ");
            for (i, col) in query.group_by.iter().enumerate() {
                if i > 0 { sql.push_str(", "); }
                write_ident(&mut sql, col);
            }
        }

        if let Some(having) = &query.having {
            sql.push_str(" HAVING ");
            write_where_expr(&mut sql, &mut params, having, None, Some(query.model))?;
        }

        if !query.order_by.is_empty() {
            sql.push_str(" ORDER BY ");
            for (i, clause) in query.order_by.iter().enumerate() {
                if i > 0 { sql.push_str(", "); }
                write_ident(&mut sql, clause.column);
                if clause.desc { sql.push_str(" DESC"); }
            }
        }
        if let Some(n) = query.limit {
            let _ = write!(sql, " LIMIT {n}");
        }
        if let Some(n) = query.offset {
            let _ = write!(sql, " OFFSET {n}");
        }

        Ok(CompiledStatement { sql, params })
    }

    fn compile_bulk_update(&self, query: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError> {
        if query.rows.is_empty() {
            return Err(SqlError::EmptyBulkInsert);
        }
        if query.update_columns.is_empty() {
            return Err(SqlError::EmptyUpdateSet);
        }
        let pk_field = query.model.primary_key().ok_or(SqlError::MissingPrimaryKey)?;

        // UPDATE "t" SET col1 = __data.col1, col2 = __data.col2
        // FROM (VALUES ($1, $2, $3), ($4, $5, $6)) AS __data(pk, col1, col2)
        // WHERE "t".pk = __data.pk
        let mut sql = String::from("UPDATE ");
        let mut params: Vec<SqlValue> = Vec::new();
        write_ident(&mut sql, query.model.table);
        sql.push_str(" SET ");
        let mut first = true;
        for col in &query.update_columns {
            if !first { sql.push_str(", "); }
            first = false;
            write_ident(&mut sql, col);
            sql.push_str(" = __data.");
            write_ident(&mut sql, col);
        }
        sql.push_str(" FROM (VALUES ");
        let col_count = 1 + query.update_columns.len(); // pk + update cols
        let mut first_row = true;
        for row in &query.rows {
            if !first_row { sql.push_str(", "); }
            first_row = false;
            sql.push('(');
            for (i, val) in row.iter().enumerate() {
                if i > 0 { sql.push_str(", "); }
                params.push(val.clone());
                let _ = write!(sql, "${}", params.len());
            }
            sql.push(')');
        }
        sql.push_str(") AS __data(");
        write_ident(&mut sql, pk_field.column);
        for col in &query.update_columns {
            sql.push_str(", ");
            write_ident(&mut sql, col);
        }
        sql.push_str(") WHERE ");
        write_ident(&mut sql, query.model.table);
        sql.push('.');
        write_ident(&mut sql, pk_field.column);
        sql.push_str(" = __data.");
        write_ident(&mut sql, pk_field.column);

        let _ = col_count; // suppress unused warning
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

        if let Some(conflict) = &query.on_conflict {
            write_conflict_clause(&mut sql, conflict);
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

        if let Some(conflict) = &query.on_conflict {
            write_conflict_clause(&mut sql, conflict);
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

        write_where(&mut sql, &mut params, &query.where_clause, Some(query.model))?;

        Ok(CompiledStatement { sql, params })
    }

    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        let mut sql = String::from("DELETE FROM ");
        let mut params: Vec<SqlValue> = Vec::new();
        write_ident(&mut sql, query.model.table);

        write_where(&mut sql, &mut params, &query.where_clause, Some(query.model))?;

        Ok(CompiledStatement { sql, params })
    }
}

fn write_where(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    where_clause: &WhereExpr,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    if where_clause.is_empty() {
        return Ok(());
    }
    sql.push_str(" WHERE ");
    write_where_expr(sql, params, where_clause, None, model)
}

/// Render a [`WhereExpr`] at the top of a `WHERE` clause — no outer
/// parens. Children that are themselves composites get parenthesized
/// in [`write_where_expr`] so precedence between AND and OR survives
/// nesting.
fn write_where_expr(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    expr: &WhereExpr,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => {
            write_filter_qualified(sql, params, filter, qualify_with, model)
        }
        WhereExpr::And(items) => {
            write_joined(sql, params, items, " AND ", qualify_with, model)
        }
        WhereExpr::Or(items) => {
            if items.is_empty() {
                return Err(SqlError::EmptyOrBranch);
            }
            write_joined(sql, params, items, " OR ", qualify_with, model)
        }
        WhereExpr::Not(child) => {
            sql.push_str("NOT (");
            write_where_expr(sql, params, child, qualify_with, model)?;
            sql.push(')');
            Ok(())
        }
    }
}

fn write_joined(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    items: &[WhereExpr],
    sep: &str,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let mut first = true;
    for child in items {
        if !first {
            sql.push_str(sep);
        }
        first = false;
        write_child(sql, params, child, qualify_with, model)?;
    }
    Ok(())
}

/// Render a sub-expression. Predicates emit bare; `And`/`Or` get
/// wrapped in parens so a mixed tree like
/// `And(Predicate(a), Or(Predicate(b), Predicate(c)))` becomes
/// `a AND (b OR c)` instead of `a AND b OR c` (which SQL would
/// regroup as `a AND b OR c` = `(a AND b) OR c`).
fn write_child(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    expr: &WhereExpr,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => {
            write_filter_qualified(sql, params, filter, qualify_with, model)
        }
        WhereExpr::And(_) | WhereExpr::Or(_) | WhereExpr::Not(_) => {
            sql.push('(');
            write_where_expr(sql, params, expr, qualify_with, model)?;
            sql.push(')');
            Ok(())
        }
    }
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
    sql.push_str(" WHERE ");
    if has_where {
        write_where_expr(sql, params, where_clause, qualify_with, model)?;
    }
    if has_search {
        let s = search.expect("checked above");
        if has_where {
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
        Op::NotLike => {
            sql.push_str(" NOT LIKE ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::ILike => {
            sql.push_str(" ILIKE ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::NotILike => {
            sql.push_str(" NOT ILIKE ");
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
        Op::NotIn => {
            let SqlValue::List(elements) = &filter.value else {
                return Err(SqlError::InRequiresList);
            };
            if elements.is_empty() {
                return Err(SqlError::EmptyInList);
            }
            sql.push_str(" NOT IN (");
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
        Op::Between => {
            let SqlValue::List(bounds) = &filter.value else {
                return Err(SqlError::BetweenRequiresTwoElementList);
            };
            if bounds.len() != 2 {
                return Err(SqlError::BetweenRequiresTwoElementList);
            }
            sql.push_str(" BETWEEN ");
            push_param_typed(sql, params, bounds[0].clone(), cast);
            sql.push_str(" AND ");
            push_param_typed(sql, params, bounds[1].clone(), cast);
        }
        Op::IsNull => {
            let SqlValue::Bool(is_null) = filter.value else {
                return Err(SqlError::IsNullRequiresBool);
            };
            sql.push_str(if is_null { " IS NULL" } else { " IS NOT NULL" });
        }
        Op::IsDistinctFrom => {
            sql.push_str(" IS DISTINCT FROM ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::IsNotDistinctFrom => {
            sql.push_str(" IS NOT DISTINCT FROM ");
            push_param_typed(sql, params, filter.value.clone(), cast);
        }
        Op::JsonContains => {
            let SqlValue::Json(_) = &filter.value else {
                return Err(SqlError::JsonOpRequiresJson);
            };
            params.push(filter.value.clone());
            let _ = write!(sql, " @> ${}::jsonb", params.len());
        }
        Op::JsonContainedBy => {
            let SqlValue::Json(_) = &filter.value else {
                return Err(SqlError::JsonOpRequiresJson);
            };
            params.push(filter.value.clone());
            let _ = write!(sql, " <@ ${}::jsonb", params.len());
        }
        Op::JsonHasKey => {
            let SqlValue::String(_) = &filter.value else {
                return Err(SqlError::JsonKeyRequiresString);
            };
            params.push(filter.value.clone());
            let _ = write!(sql, " ? ${}", params.len());
        }
        Op::JsonHasAnyKey => {
            let SqlValue::List(keys) = &filter.value else {
                return Err(SqlError::JsonKeysRequiresList);
            };
            // Expand as text array: col ?| ARRAY[$1, $2, ...]
            sql.push_str(" ?| ARRAY[");
            let mut first = true;
            for k in keys {
                if !first { sql.push_str(", "); }
                first = false;
                push_param_typed(sql, params, k.clone(), None);
            }
            sql.push(']');
        }
        Op::JsonHasAllKeys => {
            let SqlValue::List(keys) = &filter.value else {
                return Err(SqlError::JsonKeysRequiresList);
            };
            sql.push_str(" ?& ARRAY[");
            let mut first = true;
            for k in keys {
                if !first { sql.push_str(", "); }
                first = false;
                push_param_typed(sql, params, k.clone(), None);
            }
            sql.push(']');
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

/// Compile the WHERE / ORDER BY / LIMIT / OFFSET tail of a `SelectQuery`
/// into a `CompiledStatement`. The `sql` field starts at the first `WHERE`
/// keyword (or is empty when there are no filters/search/ordering). `params`
/// carries the bound values for the WHERE clause in order.
///
/// Used by `annotate_count_children_on` to forward the parent queryset's
/// WHERE / ORDER / LIMIT constraints into the hand-rolled aggregate SQL.
pub(crate) fn compile_where_order_tail(
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    order_by: &[OrderClause],
    limit: Option<i64>,
    offset: Option<i64>,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<CompiledStatement, SqlError> {
    let mut sql = String::new();
    let mut params: Vec<SqlValue> = Vec::new();
    write_where_with_search_qualified(&mut sql, &mut params, where_clause, search, qualify_with, model)?;
    if !order_by.is_empty() {
        sql.push_str(" ORDER BY ");
        let mut first = true;
        for clause in order_by {
            if !first { sql.push_str(", "); }
            first = false;
            if let Some(table) = qualify_with {
                write_ident(&mut sql, table);
                sql.push('.');
            }
            write_ident(&mut sql, clause.column);
            if clause.desc { sql.push_str(" DESC"); }
        }
    }
    if let Some(n) = limit {
        let _ = write!(sql, " LIMIT {n}");
    }
    if let Some(n) = offset {
        let _ = write!(sql, " OFFSET {n}");
    }
    Ok(CompiledStatement { sql, params })
}

fn write_conflict_clause(sql: &mut String, conflict: &ConflictClause) {
    match conflict {
        ConflictClause::DoNothing => sql.push_str(" ON CONFLICT DO NOTHING"),
        ConflictClause::DoUpdate { target, update_columns } => {
            sql.push_str(" ON CONFLICT (");
            let mut first = true;
            for col in target {
                if !first { sql.push_str(", "); }
                first = false;
                write_ident(sql, col);
            }
            sql.push_str(") DO UPDATE SET ");
            let mut first = true;
            for col in update_columns {
                if !first { sql.push_str(", "); }
                first = false;
                write_ident(sql, col);
                sql.push_str(" = EXCLUDED.");
                write_ident(sql, col);
            }
        }
    }
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
