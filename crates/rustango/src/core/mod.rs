//! Core types for rustango.
//!
//! This crate is dependency-light on purpose: no async, no DB drivers, no proc-macros.
//! Anything that needs to be referenced by both the macro output and the runtime lives here.

pub mod case;
mod column;
mod error;
mod expr;
mod field_type;
pub mod funcs;
pub mod joins;
mod query;
mod schema;
pub mod subquery;
mod validate;
mod value;

pub use case::{case, value, CaseBuilder};
pub use column::{Column, TypedAssignment, TypedExpr, TypedFilter};
pub use error::QueryError;
pub use expr::{BinOp, CaseBranch, Expr, ScalarFn, F};
pub use field_type::FieldType;
pub use query::{
    AggregateExpr, AggregateQuery, Assignment, BulkInsertQuery, BulkUpdateQuery, ColumnFilter,
    ConflictClause, CountQuery, DeleteQuery, Filter, InsertQuery, Join, JoinKind, Op, OrderClause,
    SearchClause, SelectQuery, UpdateQuery, WhereExpr,
};
pub use schema::{
    infer_app_label_from_module_path, AdminConfig, CheckConstraint, CompositeFkRelation,
    FieldSchema, Fieldset, GenericRelation, IndexSchema, M2MRelation, Model, ModelEntry,
    ModelSchema, ModelScope, Relation,
};
pub use validate::validate_value;
pub use value::SqlValue;

/// Re-exported so `#[derive(Model)]` output can name `inventory` without
/// requiring downstream crates to add their own dependency on it.
#[doc(hidden)]
pub use inventory;

/// Returns the crate version. Used by the workspace smoke test.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
