//! Per-row permission hooks: one hook per (model, add/change/delete/view)
//! pair, run by the admin and by every [`ViewSet`](crate::viewset::ViewSet).
//!
//! An inventory registry of `(table, action, fn(&Parts, Option<&Value>) -> bool)`
//! entries. The admin's create, detail, edit, update and delete handlers and
//! the ViewSet's create, retrieve, update and destroy all consult it: a
//! `"view"` refusal reads as a 404, any other as a 403.
//!
//! A `"view"` hook also drops denied rows from lists. A list runs it over
//! every row its filters and search match, so totals leave denied rows out;
//! past a scan cap it withholds those. A [`register_queryset_scope!`]
//! filter does the same in SQL and scales.
//!
//! `row` is `None` for a collection-level check, such as "may this
//! user reach the add form?", and `Some(&json)` for a row check: every
//! scalar field of the stored row, keyed by field name.
//!
//! ## Usage
//!
//! ```ignore
//! use axum::http::request::Parts;
//! use rustango::object_permissions::Actor;
//! use serde_json::Value;
//!
//! // Only the row's owner may edit or delete it.
//! fn owner_only(parts: &Parts, row: Option<&Value>) -> bool {
//!     let Some(row) = row else { return true };
//!     let owner = row.get("owner_id").and_then(Value::as_i64);
//!     matches!(Actor::of(parts), Actor::TenantUser { id, .. } if Some(id) == owner)
//! }
//! rustango::register_object_permission!("blog_post", "change", owner_only);
//! rustango::register_object_permission!("blog_post", "delete", owner_only);
//! ```
//!
//! ## Compose
//!
//! Every hook on the same `(table, action)` must return `true` for the
//! action to be allowed. The first `false` wins.
//!
//! ## Action names
//!
//! Plain strings, so a new action such as `"approve"` needs no change
//! here. The built-in handlers use `"add"`, `"change"`, `"delete"` and
//! `"view"`; a custom view can call [`is_allowed`] with a name of its own.

mod actor;
mod scope;

#[cfg(feature = "tenancy")]
pub(crate) use actor::RequestTenant;
pub use actor::{is_superuser, Actor};
pub use scope::{for_table as queryset_scopes_for, QuerySetScope, QuerySetScopeFn};
pub(crate) use scope::{row_pk, view_hook_rows, warn_scan_cap, ViewHookRows};

use axum::http::request::Parts;
use serde_json::Value;

/// An object-permission hook.
///
/// `row` is `Some(&json)` for a row-level action and `None` for a
/// collection-level one. Return `true` to allow, `false` to deny.
///
/// A plain `fn` pointer, not `Arc<dyn Fn>`, so the registration fits
/// in inventory's const storage, as in every other registry here.
pub type ObjectPermissionFn = fn(&Parts, Option<&Value>) -> bool;

/// Who registered a hook. A ViewSet's `skip_object_permissions()` skips
/// only [`HookOrigin::App`] hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookOrigin {
    /// Registered through [`register_object_permission!`].
    App,
    /// A framework table's own rule; nothing skips it.
    #[doc(hidden)]
    Framework,
}

/// One hook registration for a table and action, collected by
/// inventory via [`register_object_permission!`].
pub struct ObjectPermission {
    /// Model table. Must equal `ModelSchema::table`.
    pub table: &'static str,
    /// Action name: `"add"`, `"change"`, `"delete"`, `"view"` or your own.
    pub action: &'static str,
    /// The predicate.
    pub check: ObjectPermissionFn,
    /// Who registered it.
    pub origin: HookOrigin,
}

inventory::collect!(ObjectPermission);

/// Which hooks a request runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hooks {
    /// Every hook and queryset scope.
    All,
    /// Only the framework's own hooks; no queryset scope.
    FrameworkOnly,
}

impl Hooks {
    fn runs(self, entry: &ObjectPermission) -> bool {
        self == Self::All || entry.origin == HookOrigin::Framework
    }

    /// `true` when every hook of `(table, action)` in this set allows.
    pub(crate) fn allows(
        self,
        table: &str,
        action: &str,
        parts: &Parts,
        row: Option<&Value>,
    ) -> bool {
        inventory::iter::<ObjectPermission>
            .into_iter()
            .filter(|e| e.table == table && e.action == action && self.runs(e))
            .all(|e| (e.check)(parts, row))
    }

    /// `true` when this set holds a hook for `(table, action)`.
    pub(crate) fn any(self, table: &str, action: &str) -> bool {
        inventory::iter::<ObjectPermission>
            .into_iter()
            .any(|e| e.table == table && e.action == action && self.runs(e))
    }

    /// Whether this set holds no queryset scope for `table`.
    pub(crate) fn scope_filters_empty(self, table: &str) -> bool {
        self == Self::FrameworkOnly || scope::for_table(table).is_empty()
    }

    /// The queryset scopes' filters for `table`, ANDed by the caller.
    pub(crate) fn scope_filters(self, table: &str, parts: &Parts) -> Vec<crate::core::Filter> {
        match self {
            Self::All => scope::for_table(table)
                .iter()
                .flat_map(|h| (h.hook)(parts))
                .collect(),
            Self::FrameworkOnly => Vec::new(),
        }
    }
}

/// `true` when every hook for `(table, action)` returns `true`, or
/// when none is registered. The first `false` wins.
#[must_use]
pub fn is_allowed(table: &str, action: &str, parts: &Parts, row: Option<&Value>) -> bool {
    Hooks::All.allows(table, action, parts, row)
}

/// `true` when any hook is registered for `(table, action)`.
#[must_use]
pub(crate) fn has_hook(table: &str, action: &str) -> bool {
    Hooks::All.any(table, action)
}

/// Register a permission predicate for one model.
///
/// `$action` is usually `"add"`, `"change"`, `"delete"` or `"view"`,
/// which the admin and ViewSets check. Any other name works too: call
/// [`is_allowed`] with it from your own code.
///
/// ```ignore
/// fn allowed(_parts: &axum::http::request::Parts, _row: Option<&serde_json::Value>) -> bool {
///     true
/// }
/// rustango::register_object_permission!("blog_post", "change", allowed);
/// ```
#[macro_export]
macro_rules! register_object_permission {
    ($table:expr, $action:expr, $check:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::object_permissions::ObjectPermission {
                table: $table,
                action: $action,
                check: {
                    const _CHECK: $crate::object_permissions::ObjectPermissionFn = $check;
                    _CHECK
                },
                origin: $crate::object_permissions::HookOrigin::App,
            }
        }
    };
}

/// The old name of [`register_object_permission!`].
#[deprecated(since = "0.62.0", note = "use `register_object_permission!`")]
#[macro_export]
macro_rules! register_admin_object_permission {
    ($table:expr, $action:expr, $check:expr $(,)?) => {
        $crate::register_object_permission!($table, $action, $check);
    };
}

/// A framework table's hook: `skip_object_permissions()` does not skip it.
macro_rules! register_framework_object_permission {
    ($table:expr, $action:expr, $check:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::object_permissions::ObjectPermission {
                table: $table,
                action: $action,
                check: {
                    const _CHECK: $crate::object_permissions::ObjectPermissionFn = $check;
                    _CHECK
                },
                origin: $crate::object_permissions::HookOrigin::Framework,
            }
        }
    };
}
pub(crate) use register_framework_object_permission;

/// Framework hook: only a superuser, never an API agent or anonymous caller.
#[cfg(feature = "sso")]
pub(crate) fn superuser_only(parts: &Parts, _: Option<&Value>) -> bool {
    is_superuser(parts)
}

/// Framework hook: a superuser's row needs a superuser.
pub(crate) fn superuser_row_needs_superuser(parts: &Parts, row: Option<&Value>) -> bool {
    let row_is_superuser = row.and_then(|r| r.get("is_superuser")).is_some_and(|v| {
        v.as_bool()
            .unwrap_or_else(|| v.as_i64().is_some_and(|n| n != 0))
    });
    !row_is_superuser || is_superuser(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> Parts {
        axum::http::Request::builder()
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn deny(_: &Parts, _: Option<&Value>) -> bool {
        false
    }
    crate::register_object_permission!("op_unit_app", "view", deny);
    register_framework_object_permission!("op_unit_fw", "view", deny);

    #[test]
    fn no_hook_allows() {
        assert!(is_allowed("nonexistent", "change", &parts(), None));
    }

    /// The opt-out drops app hooks and keeps the framework's.
    #[test]
    fn framework_only_skips_app_hooks() {
        let p = parts();
        assert!(!Hooks::All.allows("op_unit_app", "view", &p, None));
        assert!(Hooks::FrameworkOnly.allows("op_unit_app", "view", &p, None));
        assert!(!Hooks::FrameworkOnly.any("op_unit_app", "view"));
        assert!(!Hooks::FrameworkOnly.allows("op_unit_fw", "view", &p, None));
        assert!(Hooks::FrameworkOnly.any("op_unit_fw", "view"));
    }

    #[test]
    fn a_superuser_row_needs_a_superuser() {
        let su = serde_json::json!({"is_superuser": true});
        let su_int = serde_json::json!({"is_superuser": 1});
        let member = serde_json::json!({"is_superuser": false});
        let anon = parts();
        assert!(!superuser_row_needs_superuser(&anon, Some(&su)));
        assert!(!superuser_row_needs_superuser(&anon, Some(&su_int)));
        assert!(superuser_row_needs_superuser(&anon, Some(&member)));
        assert!(superuser_row_needs_superuser(&anon, None));
    }
}
