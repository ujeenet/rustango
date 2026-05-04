//! Postgres dialect: double-quoted identifiers, `$1`-style placeholders,
//! `BIGSERIAL` / `SERIAL` for `Auto<T>` PKs, native `BOOLEAN`,
//! `pg_advisory_lock` for migration coordination, full `RETURNING` /
//! `ON CONFLICT` / `ILIKE` / `IS DISTINCT FROM` / JSONB operator support.
//!
//! The IR-to-SQL helpers live in [`super::writers`]. This module is
//! the thin per-dialect shell: identity primitives, the
//! Postgres-specific NULL-cast table, the conflict-clause spelling,
//! and `compile_*` methods that hand off to the writers.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldType, InsertQuery, ModelSchema, Op, OrderClause, SearchClause, SelectQuery, UpdateQuery,
    WhereExpr,
};

use super::writers::{
    self, write_aggregate, write_bulk_insert, write_bulk_update_pg, write_count, write_delete,
    write_insert, write_select, write_update, Sql,
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

    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        // PostgreSQL accepts the shorter `<expr>::bigint` form.
        format!("{expr}::bigint")
    }

    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        format!("{expr}::double precision")
    }

    fn null_cast(&self, ty: FieldType) -> Option<&'static str> {
        Some(match ty {
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

    /// Postgres supports every `Op` the IR carries — `ILIKE`,
    /// `IS DISTINCT FROM`, JSONB operators, etc. (Trait default
    /// `true` is correct; the explicit override documents intent.)
    fn supports_op(&self, _op: Op) -> bool {
        true
    }

    fn write_conflict_clause(
        &self,
        sql: &mut String,
        conflict: &ConflictClause,
    ) -> Result<(), SqlError> {
        match conflict {
            ConflictClause::DoNothing => {
                sql.push_str(" ON CONFLICT DO NOTHING");
            }
            ConflictClause::DoUpdate {
                target,
                update_columns,
            } => {
                sql.push_str(" ON CONFLICT (");
                let mut first = true;
                for col in target {
                    if !first {
                        sql.push_str(", ");
                    }
                    first = false;
                    write_pg_ident(sql, col);
                }
                sql.push_str(") DO UPDATE SET ");
                let mut first = true;
                for col in update_columns {
                    if !first {
                        sql.push_str(", ");
                    }
                    first = false;
                    write_pg_ident(sql, col);
                    sql.push_str(" = EXCLUDED.");
                    write_pg_ident(sql, col);
                }
            }
        }
        Ok(())
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

    // ---- compilation: thin shells over `writers::*` ----

    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_select(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_count(&self, query: &CountQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_count(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_aggregate(&self, query: &AggregateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_aggregate(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::with_capacity(self, query.values.len());
        write_insert(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_bulk_insert(
        &self,
        query: &BulkInsertQuery,
    ) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::with_capacity(self, query.columns.len() * query.rows.len());
        write_bulk_insert(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_update(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_delete(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_bulk_update(&self, query: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_bulk_update_pg(&mut b, query)?;
        Ok(b.finish())
    }
}

/// Direct Postgres-quoted identifier writer used by
/// [`Postgres::write_conflict_clause`]. The conflict clause writes
/// directly into a `&mut String` (it's part of the `Dialect` trait
/// surface, not a `Sql<'_>` builder), so we need a small helper that
/// doesn't go through the builder.
fn write_pg_ident(sql: &mut String, name: &str) {
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

/// Compile the WHERE / ORDER BY / LIMIT / OFFSET tail of a `SelectQuery`
/// into a `CompiledStatement`. The `sql` field starts at the first `WHERE`
/// keyword (or is empty when there are no filters/search/ordering). `params`
/// carries the bound values for the WHERE clause in order.
///
/// Used by `annotate_count_children_on` to forward the parent queryset's
/// WHERE / ORDER / LIMIT constraints into the hand-rolled aggregate SQL.
///
/// This is the Postgres-typed shim — the underlying writer is dialect-
/// agnostic, so [`super::writers::compile_where_order_tail`] takes a
/// `&dyn Dialect` and is reusable from a `MySql` executor when batch 5
/// migrates `annotate_count_children` to `&Pool`.
///
/// # Errors
/// As [`super::writers::compile_where_order_tail`].
pub(crate) fn compile_where_order_tail(
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    order_by: &[OrderClause],
    limit: Option<i64>,
    offset: Option<i64>,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<CompiledStatement, SqlError> {
    writers::compile_where_order_tail(
        DIALECT,
        where_clause,
        search,
        order_by,
        limit,
        offset,
        qualify_with,
        model,
    )
}
