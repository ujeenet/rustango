//! Django-shape `ModelAdmin.has_{add,change,delete,view}_permission`
//! — issue #361.
//!
//! Django's admin invokes four per-object permission predicates
//! around the write paths:
//!
//! - `has_add_permission(request)` — gate `POST /<model>` (create)
//! - `has_change_permission(request, obj=None)` — gate the change
//!   form + `POST /<model>/<pk>` (update). With `obj=None` it's a
//!   collection-level test ("can the user reach the edit form?");
//!   with `obj=<row>` it's per-row.
//! - `has_delete_permission(request, obj=None)` — same shape but
//!   for delete.
//! - `has_view_permission(request, obj=None)` — gate the detail
//!   view.
//!
//! Returning `False` raises a `PermissionDenied` (HTTP 403). This
//! is the layer Django apps use for ownership scoping, soft-locks,
//! and per-record audit-restricted access.
//!
//! rustango already gates list / write routes via the
//! `permission_required` middleware (codename-based) and the
//! `Builder::read_only` allowlist. This module adds the per-row
//! hook layer on top: an inventory-collected registry of
//! `(table, action, fn(&Parts, Option<&Value>) -> bool)` entries.
//! The admin's detail / edit / update / delete / create handlers
//! consult the registry and return 403 when any hook for the
//! relevant action denies.
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
//! Now `/admin/blog_post/<id>/edit` and the delete POST both 403
//! when the request user doesn't own the row. The `"add"` /
//! `"view"` actions still permit by default since no hooks for
//! those actions were registered.
//!
//! ## Compose
//!
//! Multiple hooks on the same `(table, action)` ALL must return
//! `true` for the action to be allowed (AND semantics). The first
//! `false` wins — short-circuit evaluation. Hooks registered
//! against a table that isn't visible (filtered out by `show_only`)
//! still run when the route is mounted; they're cheap so the
//! invariant "every reachable route honors every hook" is worth
//! more than a tiny visibility-check optimization.
//!
//! ## Action names
//!
//! Plain `&'static str` rather than an enum so the registry is
//! open-ended — future actions (`approve`, `restore`, etc.) just
//! pass a new name without touching this module. Built-in admin
//! handlers consult `"add"` / `"change"` / `"delete"` / `"view"`;
//! custom views (`register_admin_view!`) can consult their own
//! action names manually via [`is_allowed`].

use axum::http::request::Parts;
use serde_json::Value;

/// Signature of an admin object-permission hook.
///
/// `row` is `Some(&json)` for object-level actions (change /
/// delete / view) and `None` for collection-level actions (add,
/// or a change-form access check with no row yet).
///
/// Return `true` to permit, `false` to deny. Multiple hooks
/// AND together; first `false` wins.
///
/// Plain `fn` pointer (not `Arc<dyn Fn>`) — same const-storage
/// reason as every other inventory registry in this crate.
pub type ObjectPermissionFn = fn(&Parts, Option<&Value>) -> bool;

/// One per-table-per-action hook registration. Inventory-collected
/// via [`crate::register_admin_object_permission!`].
pub struct AdminObjectPermission {
    /// Model table — must match `ModelSchema::table` exactly.
    pub table: &'static str,
    /// Action name. Built-in admin handlers consult `"add"` /
    /// `"change"` / `"delete"` / `"view"`. Custom views can pass
    /// arbitrary names.
    pub action: &'static str,
    /// The predicate.
    pub check: ObjectPermissionFn,
}

inventory::collect!(AdminObjectPermission);

/// `true` when every registered hook for `(table, action)` returns
/// `true` (or no hooks are registered). First `false` wins —
/// short-circuit. Built-in admin handlers call this with
/// the canonical action names before letting a write proceed.
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

/// Register a Django-shape `has_<action>_permission` predicate
/// scoped to one model.
///
/// `$action` is the action name — typically one of `"add"`,
/// `"change"`, `"delete"`, `"view"`. Custom values are fine; the
/// built-in admin handlers consult the canonical four, and
/// downstream code can call [`is_allowed`] with whatever name
/// it likes.
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
