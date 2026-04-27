//! The `Dialect` trait — one implementation per database backend.

use rustango_core::{InsertQuery, SelectQuery};

use crate::{CompiledStatement, SqlError};

/// Writes a dialect-neutral query IR to a parameterized statement.
pub trait Dialect {
    /// Lower a `SelectQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError`] if any filter has a value shape incompatible with
    /// its operator (see the variants for specifics).
    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower an `InsertQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyInsert`] if no columns were supplied.
    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError>;
}
