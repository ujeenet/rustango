//! `MySQL` 8.4+ dialect — backtick-quoted identifiers, `?` placeholders,
//! `BIGINT AUTO_INCREMENT` for `Auto<T>` PKs, `1`/`0` boolean literals,
//! `GET_LOCK` / `RELEASE_LOCK` for advisory locking.
//!
//! ## v0.23.0 batch status
//!
//! v0.23.0-batch2 (this batch) ships the **identity primitives** plus
//! the connection plumbing — enough for `Pool::connect("mysql://…")`
//! to succeed and for `pool.dialect().name()` / quoting / locks to be
//! correct. The query-compilation methods (`compile_select`,
//! `compile_insert`, etc.) error with
//! [`SqlError::DialectQueryCompilationNotImplemented`] until
//! v0.23.0-batch3 ports the IR-to-SQL writers off Postgres-only
//! assumptions (placeholder shape, `RETURNING`, `ON CONFLICT` →
//! `ON DUPLICATE KEY UPDATE`, etc.).
//!
//! Apps building against the `mysql` feature in batch2 can:
//! - open a `Pool::Mysql(MySqlPool)` and use it for raw `sqlx::query!`
//! - inspect `dialect().quote_ident("col")` → `` `col` ``
//! - call `dialect().acquire_session_lock_sql()` to coordinate
//!   migration runners
//!
//! What they CAN'T do until batch3:
//! - issue ORM queries (`Model::objects().filter(...).fetch(...)`) —
//!   those go through `compile_select` and surface the error variant.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, CountQuery, DeleteQuery, FieldType,
    InsertQuery, SelectQuery, UpdateQuery,
};

use super::{CompiledStatement, Dialect, SqlError};

/// The `MySQL` 8.4+ dialect. Stateless; construct with `MySql`.
#[derive(Debug, Default, Clone, Copy)]
pub struct MySql;

/// `'static` reference to the singleton [`MySql`] dialect, symmetric
/// with [`super::postgres::DIALECT`]. Used by [`crate::sql::Pool::dialect`]
/// to hand back a `&'static dyn Dialect` regardless of pool variant.
pub static DIALECT: &MySql = &MySql;

impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }

    /// `MySQL` quotes identifiers with backticks, not double quotes.
    /// Embedded backticks are doubled (the `MySQL` parser's escape rule)
    /// so the output is always a valid quoted identifier even for
    /// pathological column names.
    fn quote_ident(&self, name: &str) -> String {
        let escaped = name.replace('`', "``");
        format!("`{escaped}`")
    }

    // `?`-style placeholders are the trait default — no override needed.

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "INT AUTO_INCREMENT",
            _ => "BIGINT AUTO_INCREMENT",
        }
    }

    /// `MySQL` has no native `BOOLEAN` (the `BOOL` keyword is just an
    /// alias for `TINYINT(1)`). Emit `1`/`0` so `DEFAULT` clauses and
    /// inline comparisons match the storage shape.
    fn bool_literal(&self, b: bool) -> &'static str {
        if b { "1" } else { "0" }
    }

    // `MySQL` ≤ 8.0 has no `CREATE INDEX … ALGORITHM=INPLACE,
    // LOCK=NONE` shape that's a true equivalent of Postgres'
    // `CONCURRENTLY` — the trait default `false` is the safe call.

    // `MySQL` has no `INSERT … RETURNING` (the trait default `false`
    // is correct). The Auto<T> insert path falls back to
    // `LAST_INSERT_ID()` in batch3 when the bulk_insert writer
    // gains its dialect-aware shape.

    // ---- advisory locks ----

    /// `MySQL`'s `GET_LOCK(name, timeout)` is the natural analog of
    /// `pg_advisory_lock`. The lock is held for the connection's
    /// lifetime and is named (string) rather than i64-keyed — the
    /// migration runner converts its u64 key to a hex string
    /// (`mig:<hex>`) before binding, so the placeholder slot here
    /// receives a string, not a number.
    ///
    /// `-1` means "wait forever" — same semantics as
    /// `pg_advisory_lock`'s blocking call. Matches the runner's
    /// expectation of an unbounded blocking acquire.
    fn acquire_session_lock_sql(&self) -> Option<String> {
        Some(format!("SELECT GET_LOCK({}, -1)", self.placeholder(1)))
    }

    fn release_session_lock_sql(&self) -> Option<String> {
        Some(format!("SELECT RELEASE_LOCK({})", self.placeholder(1)))
    }

    // `MySQL` has no transaction-scoped advisory lock (`GET_LOCK` is
    // session-scoped). The ledger bootstrap in batch3 will fall back
    // to `CREATE TABLE IF NOT EXISTS` plus a session-lock guard around
    // the whole bootstrap path. `None` here defers that work cleanly.

    // ---- compilation (lands in batch3) ----

    fn compile_select(&self, _q: &SelectQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
    fn compile_insert(&self, _q: &InsertQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
    fn compile_bulk_insert(&self, _q: &BulkInsertQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
    fn compile_update(&self, _q: &UpdateQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
    fn compile_delete(&self, _q: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
    fn compile_count(&self, _q: &CountQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
    fn compile_aggregate(&self, _q: &AggregateQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
    fn compile_bulk_update(&self, _q: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError> {
        Err(unimpl())
    }
}

fn unimpl() -> SqlError {
    SqlError::DialectQueryCompilationNotImplemented { dialect: "mysql" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::FieldType;

    #[test]
    fn name_is_mysql() {
        assert_eq!(MySql.name(), "mysql");
    }

    #[test]
    fn quote_ident_uses_backticks() {
        assert_eq!(MySql.quote_ident("col"), "`col`");
        assert_eq!(MySql.quote_ident("schema.table"), "`schema.table`");
    }

    #[test]
    fn quote_ident_escapes_embedded_backticks() {
        // Pathological but legal — must not break the parser.
        assert_eq!(MySql.quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn placeholder_is_question_mark() {
        // n is ignored on MySQL — every slot is just `?`.
        assert_eq!(MySql.placeholder(1), "?");
        assert_eq!(MySql.placeholder(7), "?");
    }

    #[test]
    fn serial_type_uses_auto_increment() {
        assert_eq!(MySql.serial_type(FieldType::I32), "INT AUTO_INCREMENT");
        assert_eq!(MySql.serial_type(FieldType::I64), "BIGINT AUTO_INCREMENT");
    }

    #[test]
    fn bool_literal_uses_one_zero() {
        assert_eq!(MySql.bool_literal(true), "1");
        assert_eq!(MySql.bool_literal(false), "0");
    }

    #[test]
    fn does_not_support_returning() {
        assert!(!MySql.supports_returning());
    }

    #[test]
    fn does_not_support_concurrent_index() {
        assert!(!MySql.supports_concurrent_index());
    }

    #[test]
    fn session_lock_uses_get_lock() {
        let acq = MySql.acquire_session_lock_sql().unwrap();
        assert!(acq.contains("GET_LOCK"));
        assert!(acq.contains("?"));
        let rel = MySql.release_session_lock_sql().unwrap();
        assert!(rel.contains("RELEASE_LOCK"));
    }

    #[test]
    fn xact_lock_is_none() {
        // MySQL has no transaction-scoped advisory lock.
        assert!(MySql.acquire_xact_lock_sql().is_none());
    }

    #[test]
    fn compile_select_errors_with_clear_message() {
        use crate::core::ModelSchema;
        // We don't construct a real SelectQuery; just confirm the error
        // shape via a shortcut: every compile_* returns the same variant.
        let err = unimpl();
        let msg = err.to_string();
        assert!(msg.contains("mysql"));
        assert!(msg.contains("v0.23.0-batch3"));
        // Quiet the unused import warning when the test compiles
        let _ = std::any::type_name::<ModelSchema>();
    }
}
