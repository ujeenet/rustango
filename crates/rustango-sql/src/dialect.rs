//! The `Dialect` trait — one implementation per database backend.

use rustango_core::SelectQuery;

use crate::{CompiledStatement, SqlError};

/// Writes a dialect-neutral `SelectQuery` to a parameterized statement.
pub trait Dialect {
    /// Lower a `SelectQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError`] if any filter has a value shape incompatible with
    /// its operator (see the variants for specifics).
    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError>;
}
