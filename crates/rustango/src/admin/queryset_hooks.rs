//! The admin's row scope: every queryset scope of a table, plus its
//! soft-delete liveness. The scope registry moved to
//! [`crate::object_permissions`]; its old paths stay re-exported here.

use axum::http::request::Parts;

use crate::core::{Filter, ModelSchema, SelectQuery, SqlValue, WhereExpr};

pub use crate::object_permissions::queryset_scopes_for as for_table;
pub use crate::object_permissions::QuerySetScopeFn as QuerySetHookFn;

/// The old name of [`crate::object_permissions::QuerySetScope`].
#[deprecated(
    since = "0.62.0",
    note = "use `rustango::object_permissions::QuerySetScope`"
)]
pub type AdminQuerySetHook = crate::object_permissions::QuerySetScope;

/// The rows of one table a request may reach: every hook's filters,
/// ANDed. The list, by-pk reads, actions, autocomplete and facets all
/// read through one, so a row the list hides is a 404 everywhere.
pub(crate) struct RowScope(Vec<Filter>);

impl RowScope {
    /// The live rows: soft-deleted ones are out of scope (#1918).
    pub(crate) fn of(model: &'static ModelSchema, parts: &Parts) -> Self {
        Self::with_liveness(model, parts, false)
    }

    /// The soft-deleted rows, for the trash list and `restore_selected`.
    pub(crate) fn trashed(model: &'static ModelSchema, parts: &Parts) -> Self {
        Self::with_liveness(model, parts, true)
    }

    fn with_liveness(model: &'static ModelSchema, parts: &Parts, trashed: bool) -> Self {
        Self(
            crate::object_permissions::Hooks::All
                .scope_filters(model.table, parts)
                .into_iter()
                .chain(crate::soft_delete::liveness_predicate(model, trashed))
                .collect(),
        )
    }

    pub(crate) fn filters(&self) -> &[Filter] {
        &self.0
    }

    /// AND the scope into `where_clause`.
    pub(crate) fn constrain(&self, mut where_clause: WhereExpr) -> WhereExpr {
        for f in &self.0 {
            where_clause.push_and(WhereExpr::Predicate(f.clone()));
        }
        where_clause
    }

    /// `WHERE pk = ?` inside the scope, with no `LIMIT`.
    pub(crate) fn by_pk(
        &self,
        model: &'static ModelSchema,
        pk_column: &'static str,
        pk: SqlValue,
    ) -> SelectQuery {
        let q = SelectQuery::by_pk(model, pk_column, pk);
        SelectQuery {
            where_clause: self.constrain(q.where_clause),
            limit: None,
            ..q
        }
    }

    /// `WHERE pk IN (…)` inside the scope.
    pub(crate) fn by_pk_in(
        &self,
        model: &'static ModelSchema,
        pk_column: &'static str,
        pks: Vec<SqlValue>,
    ) -> SelectQuery {
        let q = SelectQuery::by_pk_in(model, pk_column, pks);
        SelectQuery {
            where_clause: self.constrain(q.where_clause),
            ..q
        }
    }
}

/// One admin write [`hidden_fk_target`] checks. On update, `stored` is the
/// row's pk and `held` the row as read, so a link it holds is kept.
pub(crate) struct LinkedWrite<'a> {
    pub(crate) values: &'a [(&'static str, SqlValue)],
    pub(crate) stored: Option<(&'static crate::core::FieldSchema, &'a SqlValue)>,
    pub(crate) held: Option<&'a serde_json::Value>,
}

/// The first of `writes` with an FK naming a target row this request cannot
/// see — outside its [`RowScope`] or refused by its "view" hook — as
/// `(index, field)`. Read on the pool, one read per target for all writes.
pub(crate) async fn hidden_fk_target(
    pool: &crate::sql::Pool,
    parts: &Parts,
    owner: &'static ModelSchema,
    writes: &[LinkedWrite<'_>],
) -> Result<Option<(usize, &'static crate::core::FieldSchema)>, crate::sql::ExecError> {
    use crate::admin::object_permissions::{has_hook, is_allowed};
    use crate::viewset::fk_target::{first_hidden_in, FkTarget, FkWrite};
    let targets: Vec<FkTarget> = owner
        .scalar_fields()
        .filter(|f| {
            writes
                .iter()
                .any(|w| w.values.iter().any(|(c, _)| *c == f.column))
        })
        .filter_map(|f| {
            FkTarget::of(f, |target| {
                let scope = RowScope::of(target, parts).0;
                let scope = scope.into_iter().map(WhereExpr::Predicate).collect();
                (scope, has_hook(target.table, "view"))
            })
        })
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    // A value the read row already holds is no new link: no query for it.
    let fresh: Vec<Vec<(&'static str, SqlValue)>> = writes
        .iter()
        .map(|w| {
            w.values
                .iter()
                .filter(|(column, value)| {
                    !w.held.is_some_and(|row| holds(owner, row, column, value))
                })
                .cloned()
                .collect()
        })
        .collect();
    let checks: Vec<FkWrite<'_>> = writes
        .iter()
        .zip(&fresh)
        .map(|(w, values)| FkWrite {
            values,
            stored: w.stored,
        })
        .collect();
    let admits =
        |t: &FkTarget, row: &serde_json::Value| is_allowed(t.model.table, "view", parts, Some(row));
    let owner_scope: Vec<WhereExpr> = RowScope::of(owner, parts)
        .0
        .into_iter()
        .map(WhereExpr::Predicate)
        .collect();
    first_hidden_in(pool, None, (owner, &owner_scope), &targets, &checks, admits).await
}

/// `row` stores `value` in `column`, read the way the edit form shows it.
fn holds(
    owner: &'static ModelSchema,
    row: &serde_json::Value,
    column: &str,
    value: &SqlValue,
) -> bool {
    owner.field_by_column(column).is_some_and(|f| {
        let shown = crate::forms::input_value::render_value_for_input_json(row, f);
        crate::forms::parse_form_value(f, Some(&shown)).is_ok_and(|v| v == *value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_table_returns_empty_when_no_registrations() {
        let v = for_table("nonexistent_table");
        assert!(v.is_empty());
    }
}
