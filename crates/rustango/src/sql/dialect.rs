//! The `Dialect` trait — one implementation per database backend.
//!
//! v0.8 promotes the trait from a query-compiler-only surface into the
//! per-dialect seam every SQL writer dispatches through. Postgres is
//! the only built-in `impl` shipped today; SQLite + MySQL slot in via
//! v0.10's slice 10.5 by adding new `impl Dialect for SqliteDialect`
//! and `impl Dialect for MySqlDialect` blocks alongside.
//!
//! The methods split into three layers:
//!
//! * **Compilation** — `compile_select` / `_insert` / `_update` /
//!   `_delete` / `_count` / `_bulk_insert`. Lower the dialect-neutral
//!   query IR (`SelectQuery`, etc.) to a parameterized
//!   [`CompiledStatement`]. Always overridden.
//! * **DDL primitives** — `quote_ident`, `placeholder`, `serial_type`,
//!   `bool_literal`, `supports_concurrent_index`, `supports_returning`.
//!   Used by `migrate::ddl` when emitting `CREATE TABLE` and friends.
//!   Most have sensible defaults (ANSI-quoted identifiers, `?`
//!   placeholders, no `RETURNING`); Postgres overrides what differs.
//! * **Identity** — `name()` for diagnostic logging.
//!
//! v0.10 will add advisory-lock helpers (`with_session_lock`,
//! `with_xact_lock`) so `migrate::runner` can dispatch through the
//! dialect instead of the current Postgres-typed direct calls.

use crate::core::{
    BulkInsertQuery, CountQuery, DeleteQuery, FieldType, InsertQuery, SelectQuery, UpdateQuery,
};

use super::{CompiledStatement, SqlError};

/// Writes a dialect-neutral query IR to a parameterized statement,
/// plus the small bag of per-dialect DDL primitives the migration
/// runner needs (identifier quoting, placeholder syntax, `SERIAL` /
/// `AUTOINCREMENT` spelling, etc.).
pub trait Dialect {
    // ====== Identity ======

    /// Short identifier for this dialect — `"postgres"`, `"sqlite"`,
    /// `"mysql"`. Used in error messages and tracing spans only;
    /// callers should not branch on the value.
    fn name(&self) -> &'static str;

    // ====== DDL primitives (default-implemented to ANSI shape) ======

    /// Quote an identifier (table or column name) with the dialect's
    /// quoting rules. Default: ANSI double-quotes — works for
    /// Postgres + SQLite; MySQL overrides to backticks. Embedded
    /// quote characters are doubled so the result is always safe.
    fn quote_ident(&self, name: &str) -> String {
        let escaped = name.replace('"', "\"\"");
        format!("\"{escaped}\"")
    }

    /// Render the `n`-th positional parameter placeholder. `n` is
    /// 1-based. Default: `?` — works for SQLite + MySQL; Postgres
    /// overrides to `$N`.
    fn placeholder(&self, n: usize) -> String {
        let _ = n;
        "?".to_owned()
    }

    /// SQL column type for an auto-incrementing PK declared as
    /// `Auto<T>` in the model. `field_type` is `FieldType::I32` or
    /// `FieldType::I64`; non-integer types hit the default path
    /// (`INTEGER` / `BIGINT`) — the macro layer rejects `Auto<T>` on
    /// non-integers anyway.
    ///
    /// Default: ANSI-leaning `BIGINT` / `INTEGER`. Postgres overrides
    /// to `BIGSERIAL` / `SERIAL`; SQLite to `INTEGER PRIMARY KEY
    /// AUTOINCREMENT`; MySQL to `BIGINT AUTO_INCREMENT`.
    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "INTEGER",
            _ => "BIGINT",
        }
    }

    /// Render a boolean literal for `DEFAULT` clauses and inline
    /// comparisons. Default: ANSI `TRUE` / `FALSE`. SQLite + MySQL
    /// override to `1` / `0` (no native boolean type).
    fn bool_literal(&self, b: bool) -> &'static str {
        if b {
            "TRUE"
        } else {
            "FALSE"
        }
    }

    /// `true` if `CREATE INDEX CONCURRENTLY` is honored. Postgres
    /// overrides to `true`; default is `false` so other dialects
    /// silently downgrade `atomic: false` migrations to a regular
    /// `CREATE INDEX` with a warning.
    fn supports_concurrent_index(&self) -> bool {
        false
    }

    /// `true` if `INSERT ... RETURNING <cols>` is honored. Postgres
    /// always; SQLite ≥ 3.35; MySQL never. Drives the macro
    /// codegen's `Auto<T>` insert path: `false` here forces the
    /// runner to do a `last_insert_id()`-style follow-up read.
    fn supports_returning(&self) -> bool {
        false
    }

    // ====== Compilation (always overridden) ======


    /// Lower a `SelectQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError`] if any filter has a value shape incompatible with
    /// its operator (see the variants for specifics).
    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower an `InsertQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyInsert`] if no columns were supplied, or
    /// [`SqlError::InsertShapeMismatch`] if `columns` and `values` differ in length.
    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower a `BulkInsertQuery` (multi-row INSERT) to one
    /// `CompiledStatement` whose VALUES list has one tuple per input row.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyBulkInsert`] if `rows` is empty (the
    /// caller should short-circuit), [`SqlError::EmptyInsert`] when
    /// `columns` is empty without `returning`, or
    /// [`SqlError::InsertShapeMismatch`] when any row's value count
    /// disagrees with `columns.len()`.
    fn compile_bulk_insert(
        &self,
        query: &BulkInsertQuery,
    ) -> Result<CompiledStatement, SqlError>;

    /// Lower an `UpdateQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyUpdateSet`] if `set` is empty, or any filter
    /// error from the WHERE clause.
    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower a `DeleteQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError`] for filter-shape errors in the WHERE clause.
    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower a `CountQuery` to a `SELECT COUNT(*) … WHERE …` statement.
    ///
    /// # Errors
    /// Returns [`SqlError`] for filter-shape errors in the WHERE clause.
    fn compile_count(&self, query: &CountQuery) -> Result<CompiledStatement, SqlError>;
}
