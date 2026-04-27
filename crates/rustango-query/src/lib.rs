//! Query layer for rustango.
//!
//! v0.1 ships a typed `QuerySet<T>` that builds an `AND`-joined `WHERE`
//! clause and compiles to the dialect-neutral `SelectQuery` IR in
//! `rustango-core`. The dynamic resolver lands in week 5.

use std::marker::PhantomData;

use rustango_core::{Filter, Model, ModelSchema, Op, QueryError, SelectQuery, SqlValue};

/// A lazy builder for a `SELECT` over `T`.
///
/// Filters are accumulated in insertion order; nothing touches the schema
/// until `compile` is called, so the builder never panics on bad input.
pub struct QuerySet<T: Model> {
    filters: Vec<RawFilter>,
    _model: PhantomData<fn() -> T>,
}

#[derive(Debug, Clone)]
struct RawFilter {
    field: String,
    op: Op,
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
            filters: Vec::new(),
            _model: PhantomData,
        }
    }

    /// Append a `WHERE field <op> value` predicate.
    ///
    /// `field` is the Rust-side field name; the column is looked up from the
    /// schema at compile time.
    #[must_use]
    pub fn filter(mut self, field: impl Into<String>, op: Op, value: impl Into<SqlValue>) -> Self {
        self.filters.push(RawFilter {
            field: field.into(),
            op,
            value: value.into(),
        });
        self
    }

    /// Sugar for `filter(field, Op::Eq, value)`.
    #[must_use]
    pub fn eq(self, field: impl Into<String>, value: impl Into<SqlValue>) -> Self {
        self.filter(field, Op::Eq, value)
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
        let filters = self
            .filters
            .into_iter()
            .map(|raw| resolve(model, raw))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SelectQuery { model, filters })
    }
}

fn resolve(model: &'static ModelSchema, raw: RawFilter) -> Result<Filter, QueryError> {
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
