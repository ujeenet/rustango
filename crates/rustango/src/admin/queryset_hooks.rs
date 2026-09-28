//! Request-aware scoping for the admin list view: a hook that reads
//! the request and narrows the rows the list may show.
//!
//! An inventory registry of `(table, fn(&Parts) -> Vec<Filter>)`
//! entries. The list view walks it per request and adds the filters to
//! its WHERE clause. Use it to hide soft-deleted rows, show only rows
//! the current user owns, or scope by tenant.
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

use crate::core::Filter;

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
