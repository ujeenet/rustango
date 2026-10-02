//! Request-aware row scoping for the admin: a hook that reads the
//! request and narrows the rows the admin may show or touch.
//!
//! An inventory registry of `(table, fn(&Parts) -> Vec<Filter>)`
//! entries. The list, by-pk pages, actions, autocomplete and facets
//! all add the filters to their WHERE clause (see `RowScope`, which also hides
//! soft-deleted rows). Use it to show only rows the current user owns, or scope by tenant.
//!
//! `manager_fn` and the `show_only` / `read_only` allowlists are the
//! compile-time equivalents; this hook sees the request.
//!
//! ## Usage
//!
//! ```ignore
//! use axum::http::request::Parts;
//! use rustango::core::{Filter, Op, SqlValue};
//!
//! fn only_published(_parts: &Parts) -> Vec<Filter> {
//!     vec![Filter::new("is_published", Op::Eq, SqlValue::Bool(true))]
//! }
//!
//! rustango::register_admin_queryset!("blog_post", only_published);
//! ```
//!
//! `/admin/blog_post` now always adds `AND is_published = true` to its
//! SELECT, whatever else the URL asks for. Several hooks on one table
//! compose: their filters are added in registration order.
//!
//! A hook returns predicates rather than a whole queryset because the
//! list view already builds its SELECT from the admin config, the
//! query params and custom filters. Adding `Filter`s to that pipeline
//! composes with search, facets, ordering, pagination and
//! `list_select_related`, none of which need to know hooks exist.
//!
//! Hooks are `fn` pointers for the same const-storage reason as
//! [`crate::admin::custom_views::CustomViewHandler`] and
//! [`crate::template_extensions::TeraFilterFn`].

use axum::http::request::Parts;

use crate::core::{Filter, ModelSchema, SelectQuery, SqlValue, WhereExpr};

/// An admin queryset hook. Takes the request [`Parts`], which hold
/// everything but the body, and returns extra [`Filter`]s for the list
/// view's WHERE clause.
///
/// A plain `fn` pointer, not `Arc<dyn Fn>`, so the registration fits
/// in inventory's const storage.
pub type QuerySetHookFn = fn(&Parts) -> Vec<Filter>;

/// One queryset hook registration, collected by inventory via
/// [`crate::register_admin_queryset!`].
pub struct AdminQuerySetHook {
    /// Model table the hook applies to. Must equal
    /// `ModelSchema::table`. A hook only runs on mounted routes, so a
    /// table hidden by `show_only` never reaches it.
    pub table: &'static str,
    /// The callable.
    pub hook: QuerySetHookFn,
}

inventory::collect!(AdminQuerySetHook);

/// Every hook registered for `table`, in registration order. The
/// filters are ANDed, so the order only shows up in the logs.
#[must_use]
pub fn for_table(table: &str) -> Vec<&'static AdminQuerySetHook> {
    inventory::iter::<AdminQuerySetHook>
        .into_iter()
        .filter(|h| h.table == table)
        .collect()
}

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
            for_table(model.table)
                .iter()
                .flat_map(|h| (h.hook)(parts))
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

/// Register an admin queryset hook for one model.
///
/// See the module docs for what it does. The hook must be a plain
/// `fn`, or a closure with no captures, that coerces to
/// [`QuerySetHookFn`].
///
/// ```ignore
/// fn only_owned(parts: &axum::http::request::Parts) -> Vec<rustango::core::Filter> {
///     let user_id = parts.extensions.get::<UserId>().copied().unwrap_or(0);
///     vec![rustango::core::Filter::new(
///         "owner_id",
///         rustango::core::Op::Eq,
///         rustango::core::SqlValue::I64(user_id),
///     )]
/// }
/// rustango::register_admin_queryset!("blog_post", only_owned);
/// ```
#[macro_export]
macro_rules! register_admin_queryset {
    ($table:expr, $hook:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::admin::queryset_hooks::AdminQuerySetHook {
                table: $table,
                hook: {
                    const _HOOK: $crate::admin::queryset_hooks::QuerySetHookFn = $hook;
                    _HOOK
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_table_returns_empty_when_no_registrations() {
        let v = for_table("nonexistent_table");
        assert!(v.is_empty());
    }

    #[test]
    fn fn_pointer_coercion_smoke_test() {
        fn _h(_p: &Parts) -> Vec<Filter> {
            Vec::new()
        }
        let _f: QuerySetHookFn = _h;
    }
}
