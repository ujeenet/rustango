//! Error types shared between the query and SQL layers.

use crate::FieldType;

/// Error raised while building or compiling a `QuerySet`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueryError {
    #[error("model `{model}` has no field `{field}`")]
    UnknownField { model: &'static str, field: String },

    #[error("field `{model}.{field}` is type {expected}, but the bound value is type {actual}")]
    TypeMismatch {
        model: &'static str,
        field: String,
        expected: FieldType,
        actual: FieldType,
    },
}
