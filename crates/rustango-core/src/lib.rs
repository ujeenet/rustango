//! Core types for rustango.
//!
//! This crate is dependency-light on purpose: no async, no DB drivers, no proc-macros.
//! Anything that needs to be referenced by both the macro output and the runtime lives here.

mod error;
mod field_type;
mod query;
mod schema;
mod value;

pub use error::QueryError;
pub use field_type::FieldType;
pub use query::{Assignment, DeleteQuery, Filter, InsertQuery, Op, SelectQuery, UpdateQuery};
pub use schema::{FieldSchema, Model, ModelEntry, ModelSchema, Relation};
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
