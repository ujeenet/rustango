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
//! A `"view"` hook also drops denied rows from the list, autocomplete,
//! FK facet names and FK cell names. It runs after the page is read, so a
//! page may show fewer rows, and it does not hide denied rows from the
//! totals, facet values and counts, date buckets or the "has next" link.
//! A `register_admin_queryset!` filter hides them everywhere.
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
//! call [`is_allowed`] with any name of its own. A bulk action registered
//! with `register_action` runs `"change"` (or the `ActionPerm` it declares)
//! and a hook named after the action.

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

/// `true` when any hook is registered for `(table, action)`.
#[must_use]
pub(crate) fn has_hook(table: &str, action: &str) -> bool {
    inventory::iter::<AdminObjectPermission>
        .into_iter()
        .any(|e| e.table == table && e.action == action)
}

/// An admin form write a superuser-only field lock applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdminWrite {
    /// The create form.
    Add,
    /// The edit form and inline rows.
    Change,
}

/// Fields only a superuser may write through the admin on `action`,
/// collected via [`crate::register_admin_superuser_fields!`]. For anyone
/// else they render locked and the write leaves them out.
pub struct AdminSuperuserFields {
    /// Model table. Must equal `ModelSchema::table`.
    pub table: &'static str,
    /// Which form write.
    pub action: AdminWrite,
    /// Field names.
    pub fields: &'static [&'static str],
}

inventory::collect!(AdminSuperuserFields);

/// The superuser-only fields of `(table, action)`.
pub(crate) fn superuser_fields(table: &str, action: AdminWrite) -> Vec<&'static str> {
    inventory::iter::<AdminSuperuserFields>
        .into_iter()
        .filter(|e| e.table == table && e.action == action)
        .flat_map(|e| e.fields.iter().copied())
        .collect()
}

/// Fields a write leaves out: `readonly_fields`, plus the superuser-only
/// ones for a non-superuser.
pub(crate) struct LockedFields {
    pub(crate) readonly: &'static [&'static str],
    pub(crate) superuser_only: Vec<&'static str>,
}

impl LockedFields {
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.readonly.contains(&name) || self.superuser_only.contains(&name)
    }

    pub(crate) fn all(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.readonly
            .iter()
            .copied()
            .chain(self.superuser_only.iter().copied())
    }
}

/// Mark fields of one model superuser-only for an admin write. The model,
/// action and field names are checked at compile time, so call it where
/// the fields are visible.
///
/// ```ignore
/// rustango::register_admin_superuser_fields!(Post, Change, [owner_id]);
/// ```
#[macro_export]
macro_rules! register_admin_superuser_fields {
    ($model:ty, $action:ident, [$($field:ident),+ $(,)?]) => {
        const _: () = {
            #[allow(dead_code)]
            fn fields_exist(m: &$model) {
                $(let _ = &m.$field;)+
            }
        };
        $crate::inventory::submit! {
            $crate::admin::object_permissions::AdminSuperuserFields {
                table: <$model as $crate::core::Model>::SCHEMA.table,
                action: $crate::admin::object_permissions::AdminWrite::$action,
                fields: &[$(stringify!($field)),+],
            }
        }
    };
}

/// The superuser grant, the password and superuser rows of a user table are
/// superuser-only (#2521). Email differs per feature, so callers add it.
macro_rules! lock_user_credentials {
    ($model:ty, $table:literal) => {
        crate::register_admin_superuser_fields!($model, Add, [is_superuser]);
        crate::register_admin_superuser_fields!($model, Change, [is_superuser, password_hash]);
        crate::register_admin_object_permission!(
            $table,
            "change",
            crate::admin::session::superuser_row_needs_superuser
        );
        crate::register_admin_object_permission!(
            $table,
            "delete",
            crate::admin::session::superuser_row_needs_superuser
        );
    };
}
pub(crate) use lock_user_credentials;

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

    #[cfg(all(feature = "tenancy", feature = "mcp"))]
    #[test]
    fn credential_owner_is_superuser_only() {
        for table in ["rustango_api_keys", "rustango_agents"] {
            assert_eq!(
                superuser_fields(table, AdminWrite::Change),
                ["user_id"],
                "{table}"
            );
            assert!(
                superuser_fields(table, AdminWrite::Add).is_empty(),
                "{table}"
            );
        }
    }

    /// Both user tables lock the superuser grant and the login credentials.
    #[cfg(all(feature = "tenancy", feature = "admin-sso"))]
    #[test]
    fn user_credentials_are_superuser_only() {
        for table in ["rustango_users", "rustango_admin_users"] {
            let mut add = superuser_fields(table, AdminWrite::Add);
            add.sort_unstable();
            assert_eq!(add, ["email", "is_superuser"], "{table}");
            let mut change = superuser_fields(table, AdminWrite::Change);
            change.sort_unstable();
            assert_eq!(
                change,
                ["email", "is_superuser", "password_hash"],
                "{table}"
            );
            for action in ["change", "delete"] {
                assert!(has_hook(table, action), "{table} {action}");
            }
        }
    }

    #[test]
    fn fn_pointer_coercion_smoke_test() {
        fn _h(_p: &Parts, _row: Option<&Value>) -> bool {
            true
        }
        let _f: ObjectPermissionFn = _h;
    }
}
