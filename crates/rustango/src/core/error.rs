//! Error types shared between the query and SQL layers.

use super::FieldType;

/// Error raised while building or compiling a `QuerySet`.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryError {
    #[error("model `{model}` has no field `{field}`")]
    UnknownField { model: &'static str, field: String },

    /// `bulk_update()` was asked to change the primary key. The key
    /// picks out each row in the `WHERE`/`CASE` join, so it cannot move.
    #[error("`bulk_update` cannot update the primary-key field `{model}.{field}`")]
    BulkUpdatePrimaryKey { model: &'static str, field: String },

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

    /// The value is not one of the `#[rustango(choices = "…")]` entries.
    #[error(
        "field `{model}.{field}` value `{value}` is not one of the declared choices: {allowed:?}"
    )]
    InvalidChoice {
        model: &'static str,
        field: String,
        value: String,
        allowed: Vec<&'static str>,
    },

    /// `#[rustango(validators = "email,url")]` named a validator that
    /// does not exist. [`crate::core::FieldSchema::validators`] lists
    /// the valid names.
    #[error("field `{model}.{field}` references unknown validator `{validator}`")]
    UnknownValidator {
        model: &'static str,
        field: String,
        validator: &'static str,
    },

    /// One of the field's [`crate::core::FieldSchema::validators`]
    /// rejected the value.
    #[error("field `{model}.{field}` validator `{validator}` rejected value: {reason}")]
    ValidatorFailed {
        model: &'static str,
        field: String,
        validator: &'static str,
        reason: String,
    },

    /// `QuerySet::select_related("foo")` could not be lowered: no such
    /// field, the field is not a `ForeignKey<T>`, the target table is
    /// not registered in `inventory`, or the target has no primary key.
    #[error("select_related(`{field}`) on model `{model}` is invalid: {reason}")]
    SelectRelatedInvalid {
        model: &'static str,
        field: String,
        reason: String,
    },

    /// `AggregateBuilder::filter(alias, op, value)` got an `op` that
    /// does not compose against an aggregate left side via
    /// [`crate::core::WhereExpr::ExprCompare`]. Comparisons and the
    /// standard predicates (`In`, `Between`, `IsNull`, `Like`, …) work;
    /// the JSON ops and null-safe equality (`IsDistinctFrom` /
    /// `IsNotDistinctFrom`) do not, because they need a `&str` left side.
    ///
    /// For those, build a `WhereExpr` yourself and pass it to
    /// [`crate::query::AggregateBuilder::having`].
    #[error(
        "HAVING auto-routing for annotation alias `{alias}` doesn't support \
         {op:?} (JSON-op family + null-safe equality). Build a `WhereExpr` \
         directly and pass it through `AggregateBuilder::having`."
    )]
    HavingOpNotSupported { alias: String, op: super::Op },

    /// `.filter("field__lookup", value)` got a suffix the parser does
    /// not know. [`crate::query::QuerySet::filter`] lists the supported
    /// set; the error message repeats it.
    #[error(
        "unknown lookup suffix `__{suffix}` on field `{field}` — \
         supported: exact, iexact, contains, icontains, startswith, \
         istartswith, endswith, iendswith, gt, gte, lt, lte, ne, in, \
         isnull, between, range, regex, iregex, trigram_similar, \
         trigram_word_similar, search, array_contains, \
         array_contained_by, array_overlap, range_contains, \
         range_contained_by, range_overlap, range_strictly_left, \
         range_strictly_right, range_adjacent"
    )]
    UnknownLookup { field: String, suffix: String },

    /// A relation-spanning lookup (`author__name`) was used where the
    /// implicit JOIN is not available: `update()`, `delete()` and
    /// `.aggregate()`. It works in `filter()`, `exclude()` and
    /// `order_by()` on a SELECT. Elsewhere add the JOIN yourself with
    /// `.join(...)` plus `col_filter`.
    #[error(
        "relation-spanning lookup `{key}` is only supported in \
         filter()/exclude()/order_by() on a SELECT; for update/delete/\
         aggregate add the join explicitly via `.join(...)`"
    )]
    RelationSpanUnsupportedHere { key: String },

    /// The value does not fit the lookup: `__in` without a list,
    /// `__isnull` without a bool, `__between` without exactly two
    /// elements.
    #[error(
        "lookup `__{suffix}` on field `{field}` requires {expected}; \
         got a value of shape {actual}"
    )]
    InvalidLookupValue {
        field: String,
        suffix: String,
        expected: &'static str,
        actual: &'static str,
    },

    /// `.values(cols)` was called with no aggregating `.annotate(...)`
    /// after it. Here `.values()` is only a GROUP BY hint, so it needs
    /// an aggregate. For plain projection use `QuerySet::values_dict`,
    /// `values_list` or `values_list_flat` instead.
    #[error(
        "AggregateBuilder::values({cols:?}) requires at least one \
         aggregating annotation (Count / Sum / Avg / Max / Min / \
         StdDev / Variance). For pure projection (no GROUP BY) use \
         `QuerySet::values_dict` / `values_list` / `values_list_flat` \
         instead (issue #22)."
    )]
    ValuesRequiresAggregate { cols: Vec<&'static str> },

    /// `.values_dict(&[])` / `.values_list(&[])` got an empty column
    /// list. Zero columns would emit `SELECT FROM …`, which every
    /// dialect rejects, so we catch it while building.
    #[error(
        "`.values_dict(...)` / `.values_list(...)` requires at least \
         one column. Pass the column names you want in the projection."
    )]
    EmptyValuesProjection,

    /// `.distinct_on(&[])` got an empty column list. It would behave
    /// like `.distinct()`, which is almost always a bug.
    #[error("`.distinct_on(...)` requires at least one column; use `.distinct()` for plain SELECT DISTINCT")]
    DistinctOnEmpty,

    /// `.distinct_on(cols)` needs those columns at the head of
    /// `ORDER BY`. The order is what makes "first row per group"
    /// deterministic.
    #[error(
        "`.distinct_on({distinct_on:?})` requires those columns at the head of `.order_by(...)`; \
         got order_by={order_by:?}"
    )]
    DistinctOnOrderByMismatch {
        distinct_on: Vec<String>,
        order_by: Vec<String>,
    },
}
