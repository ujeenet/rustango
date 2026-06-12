//! Error types shared between the query and SQL layers.

use super::FieldType;

/// Error raised while building or compiling a `QuerySet`.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum QueryError {
    #[error("model `{model}` has no field `{field}`")]
    UnknownField { model: &'static str, field: String },

    /// `bulk_update()` was asked to update the primary-key column, which
    /// identifies each row in the `WHERE`/`CASE` join and so cannot be
    /// reassigned. Mirrors Django's
    /// `ValueError("bulk_update() cannot be used with primary key fields.")`.
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

    /// `#[rustango(choices = "…")]` declared a closed set of allowed
    /// values and the bound value isn't one of them. Mirrors Django's
    /// `ValidationError` raised on save when `choices` is set.
    #[error(
        "field `{model}.{field}` value `{value}` is not one of the declared choices: {allowed:?}"
    )]
    InvalidChoice {
        model: &'static str,
        field: String,
        value: String,
        allowed: Vec<&'static str>,
    },

    /// `#[rustango(validators = "email,url")]` named a validator that's
    /// not in the dispatch table. Names must match one of the entries
    /// documented on [`crate::core::FieldSchema::validators`].
    #[error("field `{model}.{field}` references unknown validator `{validator}`")]
    UnknownValidator {
        model: &'static str,
        field: String,
        validator: &'static str,
    },

    /// One of the named [`crate::core::FieldSchema::validators`] rejected
    /// the bound value. Mirrors Django's `ValidationError` raised on
    /// save when a custom validator on `Field.validators=[]` fails.
    #[error("field `{model}.{field}` validator `{validator}` rejected value: {reason}")]
    ValidatorFailed {
        model: &'static str,
        field: String,
        validator: &'static str,
        reason: String,
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
    /// an `op` that doesn't compose against an aggregate LHS via
    /// [`crate::core::WhereExpr::ExprCompare`]. After issue #87,
    /// the supported set is the binary-comparison ops (`Eq`/`Ne`/
    /// `Lt`/`Lte`/`Gt`/`Gte`) plus the SQL-92 standard predicates
    /// (`In`/`NotIn`, `Between`, `IsNull`, `Like`/`NotLike`,
    /// `ILike`/`NotILike`). The JSON-op family + null-safe equality
    /// (`IsDistinctFrom` / `IsNotDistinctFrom`) still need
    /// dialect-specific writers that take a `&str` for the LHS,
    /// so they're rejected here.
    ///
    /// Drop into [`crate::query::AggregateBuilder::having`] with a
    /// pre-built `WhereExpr` if you really need one of the
    /// remaining ops against an aggregate.
    #[error(
        "HAVING auto-routing for annotation alias `{alias}` doesn't support \
         {op:?} (JSON-op family + null-safe equality). Build a `WhereExpr` \
         directly and pass it through `AggregateBuilder::having`."
    )]
    HavingOpNotSupported { alias: String, op: super::Op },

    /// Django-shape `.filter("field__lookup", value)` got a lookup
    /// suffix the parser doesn't recognize. Issue #71. The supported
    /// set (exact / iexact / contains / icontains / startswith /
    /// istartswith / endswith / iendswith / gt / gte / lt / lte / ne
    /// / in / isnull / between / range) is documented on
    /// [`crate::query::QuerySet::filter`]. Chained lookups
    /// (`author__name__icontains`) aren't supported in v1.
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

    /// Django-shape `.filter("field__lookup", value)` got a value
    /// whose shape doesn't fit the chosen lookup. Issue #71.
    /// Examples: `__in` with a non-list, `__isnull` with a
    /// non-bool, `__between` with a list that isn't exactly 2
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

    /// `.values(cols)` was called without a subsequent aggregating
    /// `.annotate(name, ...)`. Issue #75 v1 supports `.values()` only
    /// as a GROUP BY hint paired with an aggregate annotation; pure
    /// projection (returning `Vec<HashMap<String, SqlValue>>` with
    /// only the requested columns) needs a separate writer path and
    /// is queued for a follow-up. Until then, use the typed
    /// `QuerySet::fetch(...)` path to read whole rows, or build an
    /// `AggregateQuery` directly if you need a custom SELECT shape.
    #[error(
        "AggregateBuilder::values({cols:?}) requires at least one \
         aggregating annotation (Count / Sum / Avg / Max / Min / \
         StdDev / Variance). For pure projection (no GROUP BY) use \
         `QuerySet::values_dict` / `values_list` / `values_list_flat` \
         instead (issue #22)."
    )]
    ValuesRequiresAggregate { cols: Vec<&'static str> },

    /// `.values_dict(&[])` / `.values_list(&[])` was called with an
    /// empty column list. Issue #22. A projection with zero columns
    /// would emit `SELECT FROM …` which every dialect rejects as a
    /// syntax error; we surface the problem at builder time with a
    /// clear message instead.
    #[error(
        "`.values_dict(...)` / `.values_list(...)` requires at least \
         one column. Pass the column names you want in the projection."
    )]
    EmptyValuesProjection,

    /// `.distinct_on(&[])` was called with an empty column list — would
    /// degenerate to `.distinct()` and is almost always a bug. Issue
    /// #264 / T1.2.
    #[error("`.distinct_on(...)` requires at least one column; use `.distinct()` for plain SELECT DISTINCT")]
    DistinctOnEmpty,

    /// `.distinct_on(cols)` was called but the queryset's `ORDER BY`
    /// doesn't lead with those columns. Django enforces the same
    /// constraint at runtime — the order is what makes the
    /// "first row per group" deterministic. Issue #264 / T1.2.
    #[error(
        "`.distinct_on({distinct_on:?})` requires those columns at the head of `.order_by(...)`; \
         got order_by={order_by:?}"
    )]
    DistinctOnOrderByMismatch {
        distinct_on: Vec<String>,
        order_by: Vec<String>,
    },
}
