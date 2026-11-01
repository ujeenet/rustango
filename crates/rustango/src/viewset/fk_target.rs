//! A write may only point an FK at a target row the writer can see.
//! Shared by the ViewSet and the admin; each brings its own scope.

use std::collections::HashSet;

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

    /// What a read of the target needs: the whole row when gated.
    fn fields(&self) -> Vec<&'static FieldSchema> {
        if self.gated {
            self.model.scalar_fields().collect()
        } else {
            vec![self.on]
        }
    }

    /// `SELECT` of the target row `value` names, inside the scope.
    fn probe(&self, value: &SqlValue) -> SelectQuery {
        let mut q = SelectQuery::by_pk(self.model, self.on.column, value.clone());
        q.where_clause = super::narrow(q.where_clause, self.scope.clone());
        q
    }
}

/// One write [`first_hidden_in`] checks: its FK values and, on update, the
/// owner row's pk, so a value it already holds is kept.
pub(crate) struct FkWrite<'a> {
    pub(crate) values: &'a [(&'static str, SqlValue)],
    pub(crate) stored: Option<(&'static FieldSchema, &'a SqlValue)>,
}

/// The first FK among `values` naming a row `targets` hide, read on `tx`
/// when the write holds one. On update, `stored` is the owner row's pk: a
/// value it already holds inside `owner`'s scope is kept. `admits` judges
/// a gated target's row.
pub(crate) async fn first_hidden(
    pool: &Pool,
    tx: Option<&mut PoolTx<'_>>,
    owner: (&'static ModelSchema, &[WhereExpr]),
    targets: &[FkTarget],
    values: &[(&'static str, SqlValue)],
    stored: Option<(&'static FieldSchema, &SqlValue)>,
    admits: impl Fn(&FkTarget, &Value) -> bool,
) -> Result<Option<&'static FieldSchema>, ExecError> {
    let writes = [FkWrite { values, stored }];
    let hidden = first_hidden_in(pool, tx, owner, targets, &writes, admits).await?;
    Ok(hidden.map(|(_, field)| field))
}

/// [`first_hidden`] over many writes: `(index, field)` of the first hidden
/// link. Each target is read once for every value the writes name; only a
/// value that read did not admit is re-checked one by one, as before.
pub(crate) async fn first_hidden_in(
    pool: &Pool,
    mut tx: Option<&mut PoolTx<'_>>,
    (owner, owner_scope): (&'static ModelSchema, &[WhereExpr]),
    targets: &[FkTarget],
    writes: &[FkWrite<'_>],
    admits: impl Fn(&FkTarget, &Value) -> bool,
) -> Result<Option<(usize, &'static FieldSchema)>, ExecError> {
    let linked = |w: &FkWrite<'_>, t: &FkTarget| {
        w.values
            .iter()
            .filter(|(c, v)| *c == t.field.column && !matches!(v, SqlValue::Null))
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    // Per target, the keys one IN read admitted.
    let mut seen: Vec<HashSet<String>> = Vec::with_capacity(targets.len());
    for target in targets {
        let mut keys = HashSet::new();
        let mut asked = Vec::new();
        for value in writes.iter().flat_map(|w| linked(w, target)) {
            if keys.insert(value.to_display_string()) {
                asked.push(value);
            }
        }
        keys.clear();
        if !asked.is_empty() {
            let mut q = SelectQuery::by_pk_in(target.model, target.on.column, asked);
            q.where_clause = super::narrow(q.where_clause, target.scope.clone());
            for row in read_all(pool, tx.as_deref_mut(), &q, &target.fields()).await? {
                if !target.gated || admits(target, &row) {
                    keys.insert(key_text(&row, target.on));
                }
            }
        }
        seen.push(keys);
    }
    for (idx, write) in writes.iter().enumerate() {
        for (column, value) in write.values {
            if matches!(value, SqlValue::Null) {
                continue;
            }
            let Some((t, target)) = targets
                .iter()
                .enumerate()
                .find(|(_, t)| t.field.column == *column)
            else {
                continue;
            };
            // A key spelled another way (case, type) falls to the exact reads.
            if seen[t].contains(&value.to_display_string()) {
                continue;
            }
            if let Some((pk_field, pk)) = write.stored {
                // Unchanged: resending the stored parent is not a new link.
                // Inside the owner's scope: a row the writer cannot see holds nothing.
                let mut q = SelectQuery::by_pk(owner, pk_field.column, pk.clone());
                let mut held = owner_scope.to_vec();
                held.push(WhereExpr::Predicate(Filter::new(
                    *column,
                    Op::Eq,
                    value.clone(),
                )));
                q.where_clause = super::narrow(q.where_clause, held);
                if read_one(pool, tx.as_deref_mut(), &q, &[pk_field])
                    .await?
                    .is_some()
                {
                    continue;
                }
            }
            let row = read_one(
                pool,
                tx.as_deref_mut(),
                &target.probe(value),
                &target.fields(),
            )
            .await?;
            if !row.is_some_and(|row| !target.gated || admits(target, &row)) {
                return Ok(Some((idx, target.field)));
            }
        }
    }
    Ok(None)
}

/// A read key as [`SqlValue::to_display_string`] spells it.
fn key_text(row: &Value, field: &FieldSchema) -> String {
    match row.get(field.name).or_else(|| row.get(field.column)) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

async fn read_all(
    pool: &Pool,
    tx: Option<&mut PoolTx<'_>>,
    q: &SelectQuery,
    fields: &[&'static FieldSchema],
) -> Result<Vec<Value>, ExecError> {
    match tx {
        Some(tx) => crate::sql::select_rows_as_json_tx(tx, q, fields).await,
        None => crate::sql::select_rows_as_json(pool, q, fields).await,
    }
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
