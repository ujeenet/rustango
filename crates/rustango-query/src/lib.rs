//! Query layer for rustango.
//!
//! v0.1 ships a typed `QuerySet<T>` that builds an `AND`-joined `WHERE`
//! clause and compiles to the dialect-neutral `SelectQuery` IR in
//! `rustango-core`. `UpdateBuilder<T>` mirrors the same shape for `UPDATE`,
//! and `QuerySet<T>` itself is the input to bulk delete. The dynamic
//! resolver lands in week 5.

use std::marker::PhantomData;

use rustango_core::{
    Assignment, DeleteQuery, Filter, Model, ModelSchema, Op, QueryError, SelectQuery, SqlValue,
    TypedAssignment, TypedFilter, UpdateQuery,
};

/// A lazy builder for a `SELECT` over `T`.
///
/// Filters are accumulated in insertion order; nothing touches the schema
/// until `compile` is called, so the builder never panics on bad input.
///
/// Two filter shapes are accepted and may be mixed freely:
/// * [`Self::filter`] / [`Self::eq`] — string-keyed, validated at
///   `compile` time.
/// * [`Self::where_`] — typed (`User::id.gt(10)`); the column is already
///   resolved, so it bypasses the schema lookup at compile time.
pub struct QuerySet<T: Model> {
    pending: Vec<PendingFilter>,
    limit: Option<i64>,
    offset: Option<i64>,
    _model: PhantomData<fn() -> T>,
}

/// Filter accumulator entry — keeps insertion order across string-keyed and
/// typed filter calls.
enum PendingFilter {
    /// String-keyed; resolved against the schema at `compile` time.
    Raw(RawFilter),
    /// Already resolved by a typed [`Column`](rustango_core::Column).
    Resolved(Filter),
}

#[derive(Debug, Clone)]
struct RawFilter {
    field: String,
    op: Op,
    value: SqlValue,
}

#[derive(Debug, Clone)]
struct RawAssignment {
    field: String,
    value: SqlValue,
}

impl<T: Model> Default for QuerySet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Model> QuerySet<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Cap the number of returned rows. `None` removes any previously set limit.
    #[must_use]
    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Skip the first `n` rows. Pair with [`limit`](Self::limit) for paging.
    #[must_use]
    pub fn offset(mut self, n: i64) -> Self {
        self.offset = Some(n);
        self
    }

    /// Append a `WHERE field <op> value` predicate.
    ///
    /// `field` is the Rust-side field name; the column is looked up from the
    /// schema at compile time.
    #[must_use]
    pub fn filter(mut self, field: impl Into<String>, op: Op, value: impl Into<SqlValue>) -> Self {
        self.pending.push(PendingFilter::Raw(RawFilter {
            field: field.into(),
            op,
            value: value.into(),
        }));
        self
    }

    /// Sugar for `filter(field, Op::Eq, value)`.
    #[must_use]
    pub fn eq(self, field: impl Into<String>, value: impl Into<SqlValue>) -> Self {
        self.filter(field, Op::Eq, value)
    }

    /// Append a typed predicate built via a [`Column`](rustango_core::Column)
    /// reference (e.g. `User::id.gt(10)`).
    #[must_use]
    pub fn where_(mut self, predicate: TypedFilter<T>) -> Self {
        self.pending
            .push(PendingFilter::Resolved(predicate.into_filter()));
        self
    }

    /// Validate the accumulated filters against `T::SCHEMA` and lower to
    /// the dialect-neutral `SelectQuery` IR.
    ///
    /// # Errors
    /// Returns [`QueryError::UnknownField`] if a filter names a field not
    /// present on the model, and [`QueryError::TypeMismatch`] if the bound
    /// value's type does not match the field's declared type.
    pub fn compile(self) -> Result<SelectQuery, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;
        let filters = resolve_pending(model, self.pending)?;
        Ok(SelectQuery {
            model,
            filters,
            limit: self.limit,
            offset: self.offset,
        })
    }

    /// Lower this queryset to a `DeleteQuery` — same filters, no projection.
    ///
    /// # Errors
    /// As [`QuerySet::compile`].
    pub fn compile_delete(self) -> Result<DeleteQuery, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;
        let filters = resolve_pending(model, self.pending)?;
        Ok(DeleteQuery { model, filters })
    }

    /// Start an `UpdateBuilder` carrying this queryset's filters as the WHERE clause.
    #[must_use]
    pub fn update(self) -> UpdateBuilder<T> {
        UpdateBuilder {
            qs: self,
            set: Vec::new(),
        }
    }
}

/// Accumulates `SET column = value` assignments, then compiles to an `UpdateQuery`.
///
/// Constructed via [`QuerySet::update`]. The queryset's filters become the
/// WHERE clause; an empty queryset produces an unfiltered update affecting
/// every row.
pub struct UpdateBuilder<T: Model> {
    qs: QuerySet<T>,
    set: Vec<PendingAssignment>,
}

enum PendingAssignment {
    Raw(RawAssignment),
    Resolved(Assignment),
}

impl<T: Model> UpdateBuilder<T> {
    /// Append a `SET field = value` assignment. Last write wins for repeated fields.
    #[must_use]
    pub fn set(mut self, field: impl Into<String>, value: impl Into<SqlValue>) -> Self {
        self.set.push(PendingAssignment::Raw(RawAssignment {
            field: field.into(),
            value: value.into(),
        }));
        self
    }

    /// Append a typed `SET column = value` from a [`Column`](rustango_core::Column).
    #[must_use]
    pub fn set_typed(mut self, assignment: TypedAssignment<T>) -> Self {
        self.set
            .push(PendingAssignment::Resolved(assignment.into_assignment()));
        self
    }

    /// Validate against `T::SCHEMA` and lower to an `UpdateQuery`.
    ///
    /// # Errors
    /// Returns [`QueryError::UnknownField`] if any `set` or filter names an
    /// unknown field, and [`QueryError::TypeMismatch`] if any bound value's
    /// type doesn't match the field's declared type.
    pub fn compile(self) -> Result<UpdateQuery, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;

        let assignments = self
            .set
            .into_iter()
            .map(|p| match p {
                PendingAssignment::Raw(raw) => resolve_assignment(model, raw),
                PendingAssignment::Resolved(assignment) => Ok(assignment),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let filters = resolve_pending(model, self.qs.pending)?;

        Ok(UpdateQuery {
            model,
            set: assignments,
            filters,
        })
    }
}

fn resolve_pending(
    model: &'static ModelSchema,
    pending: Vec<PendingFilter>,
) -> Result<Vec<Filter>, QueryError> {
    pending
        .into_iter()
        .map(|p| match p {
            PendingFilter::Raw(raw) => resolve_filter(model, raw),
            PendingFilter::Resolved(filter) => Ok(filter),
        })
        .collect()
}

fn resolve_filter(model: &'static ModelSchema, raw: RawFilter) -> Result<Filter, QueryError> {
    let field = model
        .field(&raw.field)
        .ok_or_else(|| QueryError::UnknownField {
            model: model.name,
            field: raw.field.clone(),
        })?;

    // `IsNull` carries a Bool sentinel (true = IS NULL, false = IS NOT NULL),
    // not a value to compare against the field — skip the type check.
    // `In` carries a List; element-by-element checking is a follow-up.
    let skip_type_check = matches!(raw.op, Op::IsNull | Op::In);

    if !skip_type_check {
        if let Some(value_ty) = raw.value.field_type() {
            if value_ty != field.ty {
                return Err(QueryError::TypeMismatch {
                    model: model.name,
                    field: raw.field,
                    expected: field.ty,
                    actual: value_ty,
                });
            }
        }
    }

    Ok(Filter {
        column: field.column,
        op: raw.op,
        value: raw.value,
    })
}

fn resolve_assignment(
    model: &'static ModelSchema,
    raw: RawAssignment,
) -> Result<Assignment, QueryError> {
    let field = model
        .field(&raw.field)
        .ok_or_else(|| QueryError::UnknownField {
            model: model.name,
            field: raw.field.clone(),
        })?;

    if let Some(value_ty) = raw.value.field_type() {
        if value_ty != field.ty {
            return Err(QueryError::TypeMismatch {
                model: model.name,
                field: raw.field,
                expected: field.ty,
                actual: value_ty,
            });
        }
    }

    Ok(Assignment {
        column: field.column,
        value: raw.value,
    })
}
