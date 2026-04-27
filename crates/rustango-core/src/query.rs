//! Dialect-neutral query IR.
//!
//! The query crate compiles a typed `QuerySet<T>` into a [`SelectQuery`].
//! The SQL crate then walks that IR and writes a parameterized statement
//! per dialect. Anything in this module is therefore visible to both.

use crate::{ModelSchema, SqlValue};

/// Comparison operator on a single column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    /// Right-hand side must be `SqlValue::List`.
    In,
    /// Case-sensitive `LIKE`. Pattern characters live inside the bound value.
    Like,
    /// Compares against `NULL`. The bound value must be `SqlValue::Bool` —
    /// `true` means `IS NULL`, `false` means `IS NOT NULL`.
    IsNull,
}

/// One predicate in a `WHERE` clause: `column <op> value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: &'static str,
    pub op: Op,
    pub value: SqlValue,
}

/// Compiled `SELECT` over a single model with zero or more `AND`-joined filters.
///
/// v0.1 selects all scalar fields of `model` and joins filters with `AND`.
/// `OR` and explicit projections land in v0.2.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub model: &'static ModelSchema,
    pub filters: Vec<Filter>,
}

/// Compiled `INSERT` of a single row.
///
/// `columns` and `values` are positional: `values[i]` binds to `columns[i]`.
/// v0.1 only emits single-row inserts; bulk inserts land in v0.2.
#[derive(Debug, Clone)]
pub struct InsertQuery {
    pub model: &'static ModelSchema,
    pub columns: Vec<&'static str>,
    pub values: Vec<SqlValue>,
}
