//! `CompiledStatement` — the result of writing a query to a specific dialect.

use rustango_core::SqlValue;

/// A parameterized statement ready to hand to a driver.
///
/// `sql` uses dialect-specific placeholder syntax (e.g. `$1` for Postgres).
/// `params` is in placeholder order: `params[i - 1]` binds `$i`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledStatement {
    pub sql: String,
    pub params: Vec<SqlValue>,
}
