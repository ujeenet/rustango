//! A write may only point an FK at a target row the writer can see.
//! Shared by the ViewSet and the admin; each brings its own scope.

use serde_json::Value;

use crate::core::{FieldSchema, Filter, ModelSchema, Op, SelectQuery, SqlValue, WhereExpr};
use crate::sql::{ExecError, Pool, PoolTx};

/// An FK whose target hides rows from this writer.
pub(crate) struct FkTarget {
    pub(crate) field: &'static FieldSchema,
    pub(crate) model: &'static ModelSchema,
    on: &'static FieldSchema,
    scope: Vec<WhereExpr>,
    /// The caller's gate judges the whole target row (the admin "view" hook).
    gated: bool,
}

impl FkTarget {
    /// `None` unless `field` is an FK and `scope` (filters, gated) of its
    /// target hides rows; others need no read.
    pub(crate) fn of(
        field: &'static FieldSchema,
        scope: impl FnOnce(&'static ModelSchema) -> (Vec<WhereExpr>, bool),
    ) -> Option<Self> {
        let (crate::core::Relation::Fk { to, on } | crate::core::Relation::O2O { to, on }) =
            field.relation?;
        let model = crate::core::ModelEntry::for_table(to)?.schema;
        let on = model.field_by_column(on)?;
        let (scope, gated) = scope(model);
        (gated || !scope.is_empty()).then_some(Self {
            field,
            model,
            on,
            scope,
            gated,
        })
    }

    /// `SELECT` of the target row `value` names, inside the scope.
    fn probe(&self, value: &SqlValue) -> SelectQuery {
        let mut q = SelectQuery::by_pk(self.model, self.on.column, value.clone());
        q.where_clause = super::narrow(q.where_clause, self.scope.clone());
        q
    }
}

/// The first FK among `values` naming a row `targets` hide, read on `tx`
/// when the write holds one. On update, `stored` is the owner row's pk: a
/// value it already holds is kept. `admits` judges a gated target's row.
pub(crate) async fn first_hidden(
    pool: &Pool,
    mut tx: Option<&mut PoolTx<'_>>,
    owner: &'static ModelSchema,
    targets: &[FkTarget],
    values: &[(&'static str, SqlValue)],
    stored: Option<(&'static FieldSchema, &SqlValue)>,
    admits: impl Fn(&FkTarget, &Value) -> bool,
) -> Result<Option<&'static FieldSchema>, ExecError> {
    for (column, value) in values {
        if matches!(value, SqlValue::Null) {
            continue;
        }
        let Some(target) = targets.iter().find(|t| t.field.column == *column) else {
            continue;
        };
        if let Some((pk_field, pk)) = stored {
            // Unchanged: resending the stored parent is not a new link.
            let mut q = SelectQuery::by_pk(owner, pk_field.column, pk.clone());
            q.where_clause = super::narrow(
                q.where_clause,
                vec![WhereExpr::Predicate(Filter::new(
                    *column,
                    Op::Eq,
                    value.clone(),
                ))],
            );
            if read_one(pool, tx.as_deref_mut(), &q, &[pk_field])
                .await?
                .is_some()
            {
                continue;
            }
        }
        let fields: Vec<&'static FieldSchema> = if target.gated {
            target.model.scalar_fields().collect()
        } else {
            vec![target.on]
        };
        let row = read_one(pool, tx.as_deref_mut(), &target.probe(value), &fields).await?;
        if !row.is_some_and(|row| !target.gated || admits(target, &row)) {
            return Ok(Some(target.field));
        }
    }
    Ok(None)
}

async fn read_one(
    pool: &Pool,
    tx: Option<&mut PoolTx<'_>>,
    q: &SelectQuery,
    fields: &[&'static FieldSchema],
) -> Result<Option<Value>, ExecError> {
    match tx {
        Some(tx) => crate::sql::select_one_row_as_json_tx(tx, q, fields).await,
        None => crate::sql::select_one_row_as_json(pool, q, fields).await,
    }
}
