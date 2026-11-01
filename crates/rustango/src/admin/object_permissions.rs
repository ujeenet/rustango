//! The admin's superuser-only field locks. The per-row hooks moved to
//! [`crate::object_permissions`]; their old paths stay re-exported here.

pub(crate) use crate::object_permissions::has_hook;
pub use crate::object_permissions::{is_allowed, ObjectPermissionFn};

/// The old name of [`crate::object_permissions::ObjectPermission`].
#[deprecated(
    since = "0.62.0",
    note = "use `rustango::object_permissions::ObjectPermission`"
)]
pub type AdminObjectPermission = crate::object_permissions::ObjectPermission;

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

/// A locked NOT NULL flag with no column default stores `false`, as an
/// unticked box would; anything else is left to the column default.
pub(crate) fn default_locked_flags(
    model: &'static crate::core::ModelSchema,
    locked: &[&'static str],
    values: &mut Vec<(&'static str, crate::core::SqlValue)>,
) {
    for f in model.scalar_fields() {
        if f.ty == crate::core::FieldType::Bool
            && !f.nullable
            && f.default.is_none()
            && locked.contains(&f.name)
            && !values.iter().any(|(c, _)| *c == f.column)
        {
            values.push((f.column, crate::core::SqlValue::Bool(false)));
        }
    }
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
        crate::object_permissions::register_framework_object_permission!(
            $table,
            "change",
            crate::object_permissions::superuser_row_needs_superuser
        );
        crate::object_permissions::register_framework_object_permission!(
            $table,
            "delete",
            crate::object_permissions::superuser_row_needs_superuser
        );
    };
}
pub(crate) use lock_user_credentials;

#[cfg(all(test, feature = "tenancy", any(feature = "mcp", feature = "admin-sso")))]
mod tests {
    use super::*;

    #[cfg(all(feature = "tenancy", feature = "mcp"))]
    #[test]
    fn credential_owner_is_superuser_only() {
        for table in ["rustango_api_keys", "rustango_agents"] {
            for action in [AdminWrite::Add, AdminWrite::Change] {
                assert_eq!(
                    superuser_fields(table, action),
                    ["user_id"],
                    "{table} {action:?}"
                );
            }
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
}
