//! Error types shared between the query and SQL layers.

use super::FieldType;

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

    #[error("field `{model}.{field}` exceeds max_length {max} (got {actual})")]
    MaxLengthExceeded {
        model: &'static str,
        field: String,
        max: u32,
        actual: u32,
    },

    #[error(
        "field `{model}.{field}` value {value} is out of range (min = {min:?}, max = {max:?})"
    )]
    OutOfRange {
        model: &'static str,
        field: String,
        value: i64,
        min: Option<i64>,
        max: Option<i64>,
    },

    /// `QuerySet::select_related("foo")` couldn't be lowered: the
    /// field doesn't exist, isn't a `ForeignKey<T>`, the target
    /// table isn't registered in `inventory`, or the target has no
    /// primary key. Slice 9.0d.
    #[error("select_related(`{field}`) on model `{model}` is invalid: {reason}")]
    SelectRelatedInvalid {
        model: &'static str,
        field: String,
        reason: String,
    },

    /// `AggregateBuilder::filter(alias, op, value)` was called with
    /// an `op` outside the binary-comparison set (`Eq`/`Ne`/`Lt`/
    /// `Lte`/`Gt`/`Gte`) — but the alias resolves to an aggregate
    /// annotation, so the predicate would be routed to `HAVING`
    /// via [`crate::core::WhereExpr::ExprCompare`], which today
    /// only supports binary comparisons. Issue #74 v1.
    ///
    /// Use [`crate::query::AggregateBuilder::having`] with a
    /// pre-built `WhereExpr` (e.g. an `Or`-tree of equalities for
    /// the `Op::In` shape) until richer ExprCompare-with-Aggregate
    /// dispatch lands as a v0.50 follow-up.
    #[error(
        "HAVING auto-routing for annotation alias `{alias}` supports only \
         binary-comparison ops (Eq / Ne / Lt / Lte / Gt / Gte); got {op:?}. \
         For `IN` / `BETWEEN` / `IS NULL` / `LIKE` / etc. against an \
         aggregate, build a `WhereExpr` directly and pass it through \
         `AggregateBuilder::having`."
    )]
    HavingOpNotSupported { alias: String, op: super::Op },
}
