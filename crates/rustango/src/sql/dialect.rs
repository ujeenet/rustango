//! The `Dialect` trait — one implementation per database backend.

use crate::core::{
    BulkInsertQuery, CountQuery, DeleteQuery, InsertQuery, SelectQuery, UpdateQuery,
};

use super::{CompiledStatement, SqlError};

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
