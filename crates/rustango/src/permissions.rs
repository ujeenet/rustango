//! Top-level permissions facade — typed convenience over the
//! existing `tenancy::permissions` engine + integration with the
//! [`crate::contenttypes`] registry from v0.15-F.1.
//!
//! The full permissions surface — `Role`, `RolePermission`,
//! `UserRole`, `UserPermission`, `has_perm` / `has_any_perm` /
//! `has_all_perms`, the `assign_role` / `grant_role_perm` /
//! `auto_create_permissions` helpers — lives in
//! [`crate::tenancy::permissions`] (it shipped under the tenancy
//! umbrella historically because the tables live in the registry
//! DB). This module re-exports it from the conceptually-cleaner
//! top-level path and adds a small typed layer that lets callers
//! reach for permissions by their `T: Model` type instead of
//! string-typing the codename:
//!
//!     // Old (still supported):
//!     rustango::tenancy::permissions::has_perm(uid, "post.change", &pool).await?;
//!
//!     // New, typed (Option G of v0.16.0):
//!     rustango::permissions::has_perm_for_model::<Post>(uid, "change", &pool).await?;
//!
//! The codename layout is unchanged (`{table}.{action}`) — the
//! typed helpers just build the string from `T::SCHEMA.table`. No
//! schema migration; legacy callers keep working.
//!
//! ## Why this is the v0.16.0 Option G "land"
//!
//! Permissions/roles substantively shipped under tenancy in earlier
//! versions; the user-visible upgrade is the typed entry point.
//! The original v0.15+ plan also called for `(content_type_id,
//! action)`-keyed permissions instead of string codenames — that
//! avoids the rename-cascade problem (renaming a model's table
//! invalidates every permission row tied to its old codename).
//! The typed helpers below are the migration step: they hide the
//! string codename behind `T: Model` so future versions can swap
//! the storage to `(content_type_id, action)` without breaking
//! callers. See `~/.claude/projects/-Users-ievgeniisvyryd-projects-rustango/memory/v015-roadmap.md`
//! for the longer-term plan.
//!
//! Requires the `tenancy` Cargo feature (the underlying tables
//! live in the tenancy bootstrap migration).

#![cfg(feature = "tenancy")]

use crate::core::Model;
use crate::sql::sqlx::PgPool;
use crate::tenancy::TenancyError;

// ----- re-export the engine for the canonical path -----

pub use crate::tenancy::permissions::{
    assign_role, auto_create_permissions, clear_user_perm, create_role, ensure_tables,
    get_or_create_role, grant_role_perm, has_all_perms, has_any_perm, has_perm,
    model_codenames, remove_role, revoke_role_perm, set_user_perm, user_permissions,
    user_roles, user_roles_qs, Role, RolePermission, UserPermission, UserRole,
};

// ----- typed entry points (v0.16.0 Option G) -----

/// Build the four standard CRUD codenames for `T` —
/// `[<table>.add, <table>.change, <table>.delete, <table>.view]`
/// resolved from `T::SCHEMA.table`. Typed counterpart of
/// [`model_codenames`] that doesn't make callers remember the table
/// name string.
#[must_use]
pub fn model_codenames_for<T: Model>() -> [String; 4] {
    model_codenames(T::SCHEMA.table)
}

/// Build a single-action codename for `T` — `<table>.<action>`.
/// `action` is conventionally one of `"add"` / `"change"` /
/// `"delete"` / `"view"` but the framework doesn't restrict it —
/// any project-defined action codename works.
#[must_use]
pub fn codename_for<T: Model>(action: &str) -> String {
    format!("{}.{action}", T::SCHEMA.table)
}

/// `has_perm(uid, "<table>.<action>", pool)` keyed off the model
/// type — one call, no string-typing.
///
/// ```ignore
/// if rustango::permissions::has_perm_for_model::<Post>(user.id, "change", &pool).await? {
///     // user can change posts
/// }
/// ```
///
/// # Errors
/// As [`has_perm`].
pub async fn has_perm_for_model<T: Model>(
    uid: i64,
    action: &str,
    pool: &PgPool,
) -> Result<bool, sqlx::Error> {
    has_perm(uid, &codename_for::<T>(action), pool).await
}

/// Grant `<table>.<action>` to `role_id` keyed off the model type.
/// Idempotent (the underlying [`grant_role_perm`] uses `ON CONFLICT
/// DO NOTHING`).
///
/// # Errors
/// As [`grant_role_perm`].
pub async fn grant_role_perm_for_model<T: Model>(
    role_id: i64,
    action: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    grant_role_perm(role_id, &codename_for::<T>(action), pool).await
}

/// Revoke `<table>.<action>` from `role_id` keyed off the model
/// type. Idempotent (no-op when the row didn't exist).
///
/// # Errors
/// As [`revoke_role_perm`].
pub async fn revoke_role_perm_for_model<T: Model>(
    role_id: i64,
    action: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    revoke_role_perm(role_id, &codename_for::<T>(action), pool).await
}

/// Set a per-user override for `<table>.<action>` keyed off the
/// model type. `granted = true` is an explicit grant; `granted =
/// false` is an explicit denial that overrides any role grant.
///
/// # Errors
/// As [`set_user_perm`].
pub async fn set_user_perm_for_model<T: Model>(
    uid: i64,
    action: &str,
    granted: bool,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    set_user_perm(uid, &codename_for::<T>(action), granted, pool).await
}

/// Clear a per-user override for `<table>.<action>` keyed off the
/// model type — the user falls back to their role-derived
/// permissions for that codename.
///
/// # Errors
/// As [`clear_user_perm`].
pub async fn clear_user_perm_for_model<T: Model>(
    uid: i64,
    action: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    clear_user_perm(uid, &codename_for::<T>(action), pool).await
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
