//! Django-shape `ModelAdmin.get_queryset(self, request)` — issue #360.
//!
//! Django lets a ModelAdmin scope its list view by overriding
//! `get_queryset(request)` — return a filtered queryset based on
//! the request user, headers, or any other per-request signal.
//! Common uses:
//!
//! - Hide soft-deleted rows.
//! - Show only the rows the current user owns.
//! - Filter by tenant when the admin runs outside the tenancy
//!   middleware.
//! - Restrict to a date window pulled from a session cookie.
//!
//! rustango's admin had `manager_fn` (compile-time QuerySet
//! shortcut) and the per-Builder `show_only` / `read_only`
//! allowlists, but no *request-aware* hook. This module ships the
//! equivalent: an inventory-collected registry of
//! `(table, fn(&Parts) -> Vec<Filter>)` entries that the admin
//! list view walks at request time and appends to the WHERE
//! clause.
//!
//! ## Usage
//!
//! ```ignore
//! use axum::http::request::Parts;
//! use rustango::core::{Filter, Op, SqlValue};
//!
//! fn only_published(_parts: &Parts) -> Vec<Filter> {
//!     vec![Filter {
//!         column: "is_published",
//!         op: Op::Eq,
//!         value: SqlValue::Bool(true),
//!     }]
//! }
//!
//! rustango::register_admin_queryset!("blog_post", only_published);
//! ```
//!
//! Now `/admin/blog_post` automatically appends `AND is_published =
//! true` to its SELECT, no matter what other filter params the URL
//! carries. Multiple registrations on the same table compose — the
//! filters from every hook are appended in registration order.
//!
//! ## Why a hook returning predicates instead of a full QuerySet?
//!
//! rustango's admin already builds its `SELECT` from the
//! `AdminConfig` + per-request query params + custom filters. The
//! hook integrates by adding more `Filter`s to that same pipeline,
//! which is the smallest possible surface — it composes with
//! search, facets, ordering, pagination, and `list_select_related`
//! without any of them needing to know hooks exist. Django's full-
//! QuerySet override is more flexible in principle but the
//! incremental-filter shape covers 95% of real use cases.
//!
//! ## Why inventory storage requires `fn` pointers
//!
//! Same const-constructible reason as
//! [`crate::admin::custom_views::CustomViewHandler`] and
//! [`crate::template_extensions::TeraFilterFn`].

use axum::http::request::Parts;

use crate::core::Filter;

/// Signature of an admin queryset hook. Receives the per-request
/// [`Parts`] (headers + uri + method + extensions, minus body) and
/// returns extra [`Filter`]s to append to the list view's WHERE
/// clause.
///
/// Plain `fn` pointer (not `Arc<dyn Fn>`) so the registration can
/// live in `inventory::submit!`'s `static` storage.
pub type QuerySetHookFn = fn(&Parts) -> Vec<Filter>;

/// One per-table queryset hook registration. Inventory-collected
/// via [`crate::register_admin_queryset!`].
pub struct AdminQuerySetHook {
    /// Model table the hook applies to — must match
    /// `ModelSchema::table` exactly. Hooks registered against a
    /// table that isn't visible (filtered out by `show_only` etc.)
    /// run only on the routes that are actually mounted.
    pub table: &'static str,
    /// The callable.
    pub hook: QuerySetHookFn,
}

inventory::collect!(AdminQuerySetHook);

/// Return every hook registered for `table`, in registration order.
/// The admin list view applies them in this order; the surface is
/// commutative for ANDed predicates so the order is observable
/// only through tracing logs.
#[must_use]
pub fn for_table(table: &str) -> Vec<&'static AdminQuerySetHook> {
    inventory::iter::<AdminQuerySetHook>
        .into_iter()
        .filter(|h| h.table == table)
        .collect()
}

/// Register an admin queryset hook scoped to one model.
///
/// See the module-level docs for semantics + use cases. The hook
/// callable must be a plain `fn` or non-capturing closure that
/// coerces to [`QuerySetHookFn`] — `inventory::submit!`'s static
/// storage doesn't accept `Arc<dyn Fn>`.
///
/// ```ignore
/// fn only_owned(parts: &axum::http::request::Parts) -> Vec<rustango::core::Filter> {
///     let user_id = parts.extensions.get::<UserId>().copied().unwrap_or(0);
///     vec![rustango::core::Filter {
///         column: "owner_id",
///         op: rustango::core::Op::Eq,
///         value: rustango::core::SqlValue::I64(user_id),
///     }]
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
