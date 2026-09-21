//! Per-row permission hooks for the admin: one hook per
//! (model, add/change/delete/view) pair.
//!
//! The admin already gates routes by permission codename and by the
//! `Builder::read_only` allowlist. This module adds a per-row layer:
//! an inventory registry of
//! `(table, action, fn(&Parts, Option<&Value>) -> bool)` entries. The
//! create, detail, edit, update and delete handlers all consult it and
//! return 403 when a hook for that action denies.
//!
//! `row` is `None` for a collection-level check, such as "may this
//! user reach the add form?", and `Some(&json)` for a row check.
//!
//! ## Usage
//!
//! ```ignore
//! use axum::http::request::Parts;
//! use serde_json::Value;
//!
//! // Only allow the row's owner to edit / delete.
//! fn owner_only(parts: &Parts, row: Option<&Value>) -> bool {
//!     let Some(row) = row else { return true; };  // no object → bail
//!     let user_id = parts.extensions.get::<i64>().copied().unwrap_or(0);
//!     row.get("owner_id").and_then(Value::as_i64) == Some(user_id)
//! }
//! rustango::register_admin_object_permission!("blog_post", "change", owner_only);
//! rustango::register_admin_object_permission!("blog_post", "delete", owner_only);
//! ```
//!
//! Now `/admin/blog_post/<id>/edit` and the delete POST both return
//! 403 when the user does not own the row. `"add"` and `"view"` stay
//! allowed, because no hook was registered for them.
//!
//! ## Compose
//!
//! Every hook on the same `(table, action)` must return `true` for the
//! action to be allowed. The first `false` wins and the rest are not
//! called. A hook still runs for a table hidden by `show_only`, as
//! long as the route is mounted.
//!
//! ## Action names
//!
//! Action names are plain strings, not an enum, so a new action such
//! as `"approve"` needs no change here. The built-in handlers use
//! `"add"`, `"change"`, `"delete"` and `"view"`. A custom view can
//! call [`is_allowed`] with any name of its own.

use axum::http::request::Parts;
use serde_json::Value;

/// An admin object-permission hook.
///
/// `row` is `Some(&json)` for a row-level action and `None` for a
/// collection-level one. Return `true` to allow, `false` to deny.
/// Hooks are ANDed together and the first `false` wins.
///
/// A plain `fn` pointer, not `Arc<dyn Fn>`, so the registration fits
/// in inventory's const storage, as in every other registry here.
pub type ObjectPermissionFn = fn(&Parts, Option<&Value>) -> bool;

/// One hook registration for a table and action, collected by
/// inventory via [`crate::register_admin_object_permission!`].
pub struct AdminObjectPermission {
    /// Model table. Must equal `ModelSchema::table`.
    pub table: &'static str,
    /// Action name. The built-in handlers use `"add"`, `"change"`,
    /// `"delete"` and `"view"`; a custom view may use any name.
    pub action: &'static str,
    /// The predicate.
    pub check: ObjectPermissionFn,
}

inventory::collect!(AdminObjectPermission);

/// `true` when every hook for `(table, action)` returns `true`, or
/// when none is registered. The first `false` wins. The built-in
/// handlers call this before they let a write through.
#[must_use]
pub fn is_allowed(table: &str, action: &str, parts: &Parts, row: Option<&Value>) -> bool {
    for entry in inventory::iter::<AdminObjectPermission> {
        if entry.table != table || entry.action != action {
            continue;
        }
        if !(entry.check)(parts, row) {
            return false;
        }
    }
    true
}

/// Register a permission predicate for one model.
///
/// `$action` is usually `"add"`, `"change"`, `"delete"` or `"view"`,
/// which the built-in handlers check. Any other name works too: call
/// [`is_allowed`] with it from your own code.
///
/// ```ignore
/// fn allowed(_parts: &axum::http::request::Parts, _row: Option<&serde_json::Value>) -> bool {
///     true
/// }
/// rustango::register_admin_object_permission!("blog_post", "change", allowed);
/// ```
#[macro_export]
macro_rules! register_admin_object_permission {
    ($table:expr, $action:expr, $check:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::admin::object_permissions::AdminObjectPermission {
                table: $table,
                action: $action,
                check: {
                    const _CHECK: $crate::admin::object_permissions::ObjectPermissionFn = $check;
                    _CHECK
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts() -> Parts {
        let req: Request<()> = Request::builder().uri("/").body(()).unwrap();
        let (p, ()) = req.into_parts();
        p
    }

    #[test]
    fn is_allowed_returns_true_when_no_hooks_registered() {
        let p = parts();
        assert!(is_allowed("nonexistent", "change", &p, None));
    }

    #[test]
    fn fn_pointer_coercion_smoke_test() {
        fn _h(_p: &Parts, _row: Option<&Value>) -> bool {
            true
        }
        let _f: ObjectPermissionFn = _h;
    }
}
