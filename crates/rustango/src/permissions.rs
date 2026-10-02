//! Permissions, reached by model type.
//!
//! The engine itself (`Role`, `has_perm`, `grant_role_perm`, and the
//! rest) lives in [`crate::tenancy::permissions`], because its tables
//! live in the registry database. This module re-exports it under a
//! shorter path and adds helpers that take `T: Model` so you do not
//! have to write the codename string by hand:
//!
//! ```ignore
//! // By string, still supported:
//! rustango::tenancy::permissions::has_perm(uid, "post.change", &pool).await?;
//!
//! // By model type:
//! rustango::permissions::has_perm_for_model::<Post>(uid, "change", &pool).await?;
//! ```
//!
//! Codenames stay `{table}.{action}`; the typed helpers just build that
//! string from `T::SCHEMA.table`.
//!
//! ## Security
//!
//! Two things defeat a check here. An active user with `is_superuser`
//! passes every one, whatever their roles say. And renaming a model's
//! table changes its codename, so old permission rows stop matching
//! and silently grant nothing; re-run `auto_create_permissions` and
//! migrate the rows after a rename.
//!
//! A check returning `false` only means "no permission". It does not
//! hide the object, so still filter your queries by tenant and owner.
//!
//! Needs the `tenancy` Cargo feature.

#![cfg(feature = "tenancy")]

use crate::core::Model;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;
use crate::tenancy::TenancyError;

// ----- re-export the engine under the canonical path -----

#[cfg(feature = "postgres")]
pub use crate::tenancy::permissions::{
    assign_role, auto_create_permissions, clear_user_perm, create_role, get_or_create_role,
    grant_role_perm, has_all_perms, has_any_perm, has_perm, remove_role, revoke_role_perm,
    set_user_perm, user_permissions, user_roles, user_roles_qs,
};
pub use crate::tenancy::permissions::{
    auto_create_permissions_pool, clear_user_perm_pool, create_role_pool, ensure_tables_pool,
    get_or_create_role_pool, grant_role_perm_pool, has_all_perms_pool, has_any_perm_pool,
    has_perm_pool, model_codenames, revoke_role_perm_pool, set_user_perm_pool,
    user_permissions_pool, user_roles_pool, user_roles_qs_pool, Role, RolePermission,
    UserPermission, UserRole,
};

// ----- typed entry points -----

/// The four CRUD codenames for `T`: `add`, `change`, `delete`, `view`,
/// each prefixed with `T::SCHEMA.table`. Typed form of
/// [`model_codenames`].
#[must_use]
pub fn model_codenames_for<T: Model>() -> [String; 4] {
    model_codenames(T::SCHEMA.table)
}

/// One codename for `T`: `<table>.<action>`. `action` is usually
/// `add`, `change`, `delete` or `view`, but any string works.
#[must_use]
pub fn codename_for<T: Model>(action: &str) -> String {
    format!("{}.{action}", T::SCHEMA.table)
}

/// [`has_perm`] for `<table>.<action>`, keyed by model type.
///
/// ```ignore
/// if rustango::permissions::has_perm_for_model::<Post>(user.id, "change", &pool).await? {
///     // user can change posts
/// }
/// ```
///
/// Postgres only. New code should call [`has_perm_for_model_pool`],
/// which works on every backend.
///
/// # Errors
/// As [`has_perm`].
#[cfg(feature = "postgres")]
pub async fn has_perm_for_model<T: Model>(
    uid: i64,
    action: &str,
    pool: &PgPool,
) -> Result<bool, sqlx::Error> {
    has_perm(uid, &codename_for::<T>(action), pool).await
}

/// [`has_perm_for_model`] over [`crate::sql::Pool`], so the same call
/// works on Postgres, SQLite and MySQL.
///
/// # Errors
/// As [`has_perm_pool`].
pub async fn has_perm_for_model_pool<T: Model>(
    uid: i64,
    action: &str,
    pool: &crate::sql::Pool,
) -> Result<bool, TenancyError> {
    has_perm_pool(uid, &codename_for::<T>(action), pool).await
}

/// Grant `<table>.<action>` to `role_id`. Safe to call twice.
///
/// Postgres only; prefer [`grant_role_perm_for_model_pool`].
///
/// # Errors
/// As [`grant_role_perm`].
#[cfg(feature = "postgres")]
pub async fn grant_role_perm_for_model<T: Model>(
    role_id: i64,
    action: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    grant_role_perm(role_id, &codename_for::<T>(action), pool).await
}

/// [`grant_role_perm_for_model`] for any backend.
///
/// # Errors
/// As [`grant_role_perm_pool`].
pub async fn grant_role_perm_for_model_pool<T: Model>(
    role_id: i64,
    action: &str,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    grant_role_perm_pool(role_id, &codename_for::<T>(action), pool).await
}

/// Revoke `<table>.<action>` from `role_id`. Does nothing if the row
/// is already gone.
///
/// Postgres only; prefer [`revoke_role_perm_for_model_pool`].
///
/// # Errors
/// As [`revoke_role_perm`].
#[cfg(feature = "postgres")]
pub async fn revoke_role_perm_for_model<T: Model>(
    role_id: i64,
    action: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    revoke_role_perm(role_id, &codename_for::<T>(action), pool).await
}

/// [`revoke_role_perm_for_model`] for any backend.
///
/// # Errors
/// As [`revoke_role_perm_pool`].
pub async fn revoke_role_perm_for_model_pool<T: Model>(
    role_id: i64,
    action: &str,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    revoke_role_perm_pool(role_id, &codename_for::<T>(action), pool).await
}

/// Override `<table>.<action>` for one user. `true` grants it;
/// `false` denies it and beats any grant the user's roles give.
///
/// Postgres only; prefer [`set_user_perm_for_model_pool`].
///
/// # Errors
/// As [`set_user_perm`].
#[cfg(feature = "postgres")]
pub async fn set_user_perm_for_model<T: Model>(
    uid: i64,
    action: &str,
    granted: bool,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    set_user_perm(uid, &codename_for::<T>(action), granted, pool).await
}

/// [`set_user_perm_for_model`] for any backend.
///
/// # Errors
/// As [`set_user_perm_pool`].
pub async fn set_user_perm_for_model_pool<T: Model>(
    uid: i64,
    action: &str,
    granted: bool,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    set_user_perm_pool(uid, &codename_for::<T>(action), granted, pool).await
}

/// Drop the per-user override for `<table>.<action>`. The user falls
/// back to what their roles give.
///
/// Postgres only; prefer [`clear_user_perm_for_model_pool`].
///
/// # Errors
/// As [`clear_user_perm`].
#[cfg(feature = "postgres")]
pub async fn clear_user_perm_for_model<T: Model>(
    uid: i64,
    action: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    clear_user_perm(uid, &codename_for::<T>(action), pool).await
}

/// [`clear_user_perm_for_model`] for any backend.
///
/// # Errors
/// As [`clear_user_perm_pool`].
pub async fn clear_user_perm_for_model_pool<T: Model>(
    uid: i64,
    action: &str,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    clear_user_perm_pool(uid, &codename_for::<T>(action), pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::Auto;

    #[derive(crate::Model)]
    #[rustango(table = "perm_t_blog_post")]
    #[allow(dead_code)]
    pub struct Post {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 200)]
        pub title: String,
    }

    #[test]
    fn model_codenames_for_resolves_through_schema() {
        let codenames = model_codenames_for::<Post>();
        assert_eq!(codenames[0], "perm_t_blog_post.add");
        assert_eq!(codenames[1], "perm_t_blog_post.change");
        assert_eq!(codenames[2], "perm_t_blog_post.delete");
        assert_eq!(codenames[3], "perm_t_blog_post.view");
    }

    #[test]
    fn codename_for_builds_table_dot_action() {
        assert_eq!(codename_for::<Post>("publish"), "perm_t_blog_post.publish");
    }
}
