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
    FieldType, InsertQuery, Op, SelectQuery, UpdateQuery,
};
#[cfg(feature = "postgres")]
use crate::core::{ModelSchema, SearchClause, WhereExpr};

#[cfg(feature = "postgres")]
use super::writers;
use super::writers::{
    write_aggregate, write_bulk_insert, write_bulk_update_pg, write_count, write_delete,
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
#[cfg(feature = "postgres")]
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

    fn column_comment_statement(&self, table: &str, column: &str, comment: &str) -> Option<String> {
        let escaped = comment.replace('\'', "''");
        Some(format!(
            "COMMENT ON COLUMN {}.{} IS '{}'",
            self.quote_ident(table),
            self.quote_ident(column),
            escaped,
        ))
    }

    fn table_comment_statement(&self, table: &str, comment: &str) -> Option<String> {
        let escaped = comment.replace('\'', "''");
        Some(format!(
            "COMMENT ON TABLE {} IS '{}'",
            self.quote_ident(table),
            escaped,
        ))
    }

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "SERIAL",
            _ => "BIGSERIAL",
        }
    }

    // #344 — CITextField. Postgres ships the `citext` extension that
    // provides a case-insensitive text type; once the extension is
    // installed, a `CITEXT` column compares case-insensitively. The
    // companion `ci_text_extension_sql` emits the one-time
    // `CREATE EXTENSION` prelude the migration runner threads in
    // ahead of the first CITEXT CREATE TABLE.
    fn ci_text_type(&self, _max_length: Option<u32>) -> String {
        // CITEXT has no length parameter; `max_length` is advisory.
        "CITEXT".to_owned()
    }

    fn ci_text_extension_sql(&self) -> Option<&'static str> {
        Some("CREATE EXTENSION IF NOT EXISTS citext;")
    }

    // Postgres has a native `BOOLEAN` type with `TRUE` / `FALSE`
    // literals — same as the trait default, no override.

    fn supports_concurrent_index(&self) -> bool {
        true
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn supports(&self, token: &str) -> bool {
        // Postgres-specific capability tokens that aren't reachable via
        // the default impl's whitelist. Extends the default; the
        // generic `window_functions` / `cte` / etc. fall through to
        // `super::Dialect::supports` via the `||`.
        matches!(
            token,
            "array_type"
                | "range_type"
                | "hstore"
                | "citext"
                | "listen_notify"
                | "notify"
                | "row_security"
                | "gin_index"
                | "gist_index"
                | "spgist_index"
                | "brin_index"
                | "unique_constraint_deferred"
                | "exclusion_constraint"
                | "tablespaces"
                | "json_path"
                | "json_query"
                // pgvector similarity search (#824). Requires the
                // `vector` extension to be installed/enabled.
                | "pgvector"
                | "vector"
                // PostGIS geometry columns (#443). Requires the
                // `postgis` extension to be installed/enabled.
                | "postgis"
                | "geometry"
                // PostGIS raster columns (#444). Requires the
                // `postgis_raster` extension.
                | "postgis_raster"
                | "raster"
        ) || self.default_supports(token)
    }

    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        // PostgreSQL accepts the shorter `<expr>::bigint` form.
        format!("{expr}::bigint")
    }

    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        format!("{expr}::double precision")
    }

    fn null_cast(&self, ty: FieldType) -> Option<&'static str> {
        // #444 — PostGIS `raster` is bound as hex text and needs an
        // explicit `::raster` cast (it has no binary input function).
        if matches!(ty, FieldType::Raster) {
            return Some("raster");
        }
        // #562 — the PG `null_cast` table was character-identical to
        // the trait-default `cast_type` table 14 variants long.
        // Delegate so the canonical token table lives in one place
        // (`Dialect::cast_type` default impl); divergence between
        // the two would silently change which `NULL::T` token PG
        // sees.
        self.cast_type(ty)
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

    fn compile_bulk_insert(&self, query: &BulkInsertQuery) -> Result<CompiledStatement, SqlError> {
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
#[cfg(feature = "postgres")]
pub(crate) fn compile_where_order_tail(
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    order_by: &[crate::core::OrderItem],
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
