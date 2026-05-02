//! Role-based permission engine for rustango tenants.
//!
//! All five tables are proper [`Model`] types — you can use the full
//! queryset ORM, the auto-admin, and `make_migrations` sees them as
//! baseline (they live in the bootstrap snapshot, not user migrations).
//!
//! ## Tables (per-tenant, created via [`ensure_tables`])
//!
//! | Model | Table | Description |
//! |---|---|---|
//! | [`Role`] | `rustango_roles` | Named groups of permissions |
//! | [`RolePermission`] | `rustango_role_permissions` | Codename → role |
//! | [`UserRole`] | `rustango_user_roles` | User → role membership |
//! | [`UserPermission`] | `rustango_user_permissions` | Per-user overrides |
//!
//! ## Codename convention
//!
//! `{model_table}.{action}` — e.g. `post.add`, `post.change`,
//! `post.delete`, `post.view`. Superusers pass every check.
//!
//! ## Effective permission resolution (single round-trip CTE)
//!
//! `has_perm(uid, codename, pool)`:
//! 1. Superuser short-circuit → true.
//! 2. Explicit per-user denial (`granted = false`) → false.
//! 3. Explicit per-user grant (`granted = true`) → true.
//! 4. Any role the user belongs to grants `codename` → true.
//! 5. Default → false.
//!
//! ## ORM usage
//!
//! ```ignore
//! // List all roles
//! let roles = Role::objects().order_by(Role::name, false).fetch(&pool).await?;
//!
//! // Which roles does a user belong to?
//! let memberships = UserRole::objects()
//!     .where_(UserRole::user_id.eq(alice.id))
//!     .fetch(&pool)
//!     .await?;
//!
//! // What codenames does a role grant?
//! let perms = RolePermission::objects()
//!     .where_(RolePermission::role_id.eq(editor_role.id))
//!     .fetch(&pool)
//!     .await?;
//! ```

use crate::core::Column as _;
use crate::sql::sqlx::{self, PgPool, Row};
use crate::sql::{Auto, Fetcher as _};
use crate::Model;

use super::error::TenancyError;

// ------------------------------------------------------------------ Models

/// A named group of permissions (Django `Group` equivalent).
///
/// Assign a user to a role via [`UserRole`]; grant codenames to a role
/// via [`RolePermission`].
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_roles",
    display = "name",
    admin(
        list_display  = "name, description",
        search_fields = "name, description",
        ordering      = "name",
    ),
)]
pub struct Role {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Human-readable name. Unique within the tenant.
    #[rustango(max_length = 150)]
    pub name: String,
    #[rustango(max_length = 500)]
    pub description: String,
}

/// One codename granted to a role. Composite key (role_id, codename)
/// enforced by DB unique constraint; surrogate `id` for ORM compat.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_role_permissions")]
pub struct RolePermission {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The role this permission belongs to.
    pub role_id: i64,
    /// Permission codename — `{table}.{action}`, e.g. `post.change`.
    #[rustango(max_length = 100)]
    pub codename: String,
}

/// Membership row linking a user to a role.
/// Surrogate `id` for ORM compat; unique constraint on `(user_id, role_id)`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_user_roles")]
pub struct UserRole {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// `rustango_users.id`
    pub user_id: i64,
    /// `rustango_roles.id`
    pub role_id: i64,
}

/// Per-user permission override. `granted = true` adds a codename
/// explicitly; `granted = false` denies it even if a role would grant it.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_user_permissions")]
pub struct UserPermission {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// `rustango_users.id`
    pub user_id: i64,
    /// Permission codename — `{table}.{action}`.
    #[rustango(max_length = 100)]
    pub codename: String,
    /// `true` = explicit grant; `false` = explicit denial.
    pub granted: bool,
}

// ------------------------------------------------------------------ ensure_tables (DDL)

const ENSURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_roles" (
    "id"          BIGSERIAL    PRIMARY KEY,
    "name"        VARCHAR(150) NOT NULL,
    "description" VARCHAR(500) NOT NULL DEFAULT '',
    CONSTRAINT "rustango_roles_name_uq" UNIQUE ("name")
);
CREATE TABLE IF NOT EXISTS "rustango_role_permissions" (
    "id"       BIGSERIAL    PRIMARY KEY,
    "role_id"  BIGINT       NOT NULL
                             REFERENCES "rustango_roles"("id")
                             ON DELETE CASCADE,
    "codename" VARCHAR(100) NOT NULL,
    CONSTRAINT "rustango_role_permissions_uq" UNIQUE ("role_id", "codename")
);
CREATE TABLE IF NOT EXISTS "rustango_user_roles" (
    "id"      BIGSERIAL PRIMARY KEY,
    "user_id" BIGINT    NOT NULL
                         REFERENCES "rustango_users"("id")
                         ON DELETE CASCADE,
    "role_id" BIGINT    NOT NULL
                         REFERENCES "rustango_roles"("id")
                         ON DELETE CASCADE,
    CONSTRAINT "rustango_user_roles_uq" UNIQUE ("user_id", "role_id")
);
CREATE TABLE IF NOT EXISTS "rustango_user_permissions" (
    "id"       BIGSERIAL    PRIMARY KEY,
    "user_id"  BIGINT       NOT NULL
                             REFERENCES "rustango_users"("id")
                             ON DELETE CASCADE,
    "codename" VARCHAR(100) NOT NULL,
    "granted"  BOOLEAN      NOT NULL DEFAULT TRUE,
    CONSTRAINT "rustango_user_permissions_uq" UNIQUE ("user_id", "codename")
);
"#;

/// Ensure all four permission tables exist in `pool`'s schema.
/// Idempotent — safe to call on every boot. The tables are framework-
/// managed (like `rustango_audit_log`) and live outside the user's
/// migration chain; `make_migrations` sees them as baseline.
///
/// # Errors
/// Driver failures from `CREATE TABLE IF NOT EXISTS`.
pub async fn ensure_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    for stmt in ENSURE_SQL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

// ------------------------------------------------------------------ has_perm (CTE query)

/// Check whether user `uid` holds permission `codename` in `pool`.
///
/// Resolution order:
/// 1. Superuser → always `true`.
/// 2. Explicit per-user denial (`granted = false`) → `false`.
/// 3. Explicit per-user grant (`granted = true`) → `true`.
/// 4. Any role the user belongs to grants `codename` → `true`.
/// 5. Default → `false`.
///
/// Single round-trip via a CTE.
///
/// # Errors
/// Driver / SQL failures.
pub async fn has_perm(uid: i64, codename: &str, pool: &PgPool) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        r#"
        WITH user_info AS (
            SELECT is_superuser
            FROM   "rustango_users"
            WHERE  id = $1 AND active = TRUE
        ),
        explicit AS (
            SELECT granted
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1 AND codename = $2
        ),
        via_role AS (
            SELECT 1
            FROM   "rustango_user_roles" ur
            JOIN   "rustango_role_permissions" rp
                   ON rp.role_id = ur.role_id
            WHERE  ur.user_id = $1 AND rp.codename = $2
            LIMIT  1
        )
        SELECT
            COALESCE((SELECT is_superuser FROM user_info), FALSE) AS is_super,
            (SELECT granted FROM explicit)                         AS explicit_grant,
            EXISTS(SELECT 1 FROM via_role)                         AS via_role
        "#,
    )
    .bind(uid)
    .bind(codename)
    .fetch_one(pool)
    .await?;

    let is_super: bool = row.try_get("is_super").unwrap_or(false);
    if is_super {
        return Ok(true);
    }
    let explicit: Option<bool> = row.try_get("explicit_grant").unwrap_or(None);
    if let Some(granted) = explicit {
        return Ok(granted);
    }
    Ok(row.try_get("via_role").unwrap_or(false))
}

/// Check whether user `uid` holds ANY of the given `codenames`.
pub async fn has_any_perm(
    uid: i64,
    codenames: &[&str],
    pool: &PgPool,
) -> Result<bool, sqlx::Error> {
    for c in codenames {
        if has_perm(uid, c, pool).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check whether user `uid` holds ALL of the given `codenames`.
pub async fn has_all_perms(
    uid: i64,
    codenames: &[&str],
    pool: &PgPool,
) -> Result<bool, sqlx::Error> {
    for c in codenames {
        if !has_perm(uid, c, pool).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

// ------------------------------------------------------------------ Role management (ORM-backed)

/// Create a role. Errors if name already exists.
pub async fn create_role(
    name: &str,
    description: &str,
    pool: &PgPool,
) -> Result<i64, TenancyError> {
    let mut role = Role {
        id: Auto::default(),
        name: name.to_owned(),
        description: description.to_owned(),
    };
    role.save_on(pool).await?;
    Ok(role.id.get().copied().unwrap_or(0))
}

/// Get an existing role by name or create one. Returns the id.
pub async fn get_or_create_role(
    name: &str,
    description: &str,
    pool: &PgPool,
) -> Result<i64, TenancyError> {
    let existing = Role::objects()
        .where_(Role::name.eq(name.to_owned()))
        .fetch(pool)
        .await?;
    if let Some(role) = existing.into_iter().next() {
        return Ok(role.id.get().copied().unwrap_or(0));
    }
    create_role(name, description, pool).await
}

/// Grant a codename to a role. No-op if already granted (fetch-then-insert).
pub async fn grant_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let existing = RolePermission::objects()
        .where_(RolePermission::role_id.eq(role_id))
        .where_(RolePermission::codename.eq(codename.to_owned()))
        .fetch(pool)
        .await?;
    if existing.is_empty() {
        let mut perm = RolePermission {
            id: Auto::default(),
            role_id,
            codename: codename.to_owned(),
        };
        perm.save_on(pool).await?;
    }
    Ok(())
}

/// Revoke a codename from a role.
pub async fn revoke_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let rows = RolePermission::objects()
        .where_(RolePermission::role_id.eq(role_id))
        .where_(RolePermission::codename.eq(codename.to_owned()))
        .fetch(pool)
        .await?;
    for row in rows {
        row.delete_on(pool).await?;
    }
    Ok(())
}

/// Assign a user to a role. No-op if already assigned.
pub async fn assign_role(
    user_id: i64,
    role_id: i64,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let existing = UserRole::objects()
        .where_(UserRole::user_id.eq(user_id))
        .where_(UserRole::role_id.eq(role_id))
        .fetch(pool)
        .await?;
    if existing.is_empty() {
        let mut membership = UserRole { id: Auto::default(), user_id, role_id };
        membership.save_on(pool).await?;
    }
    Ok(())
}

/// Remove a user from a role.
pub async fn remove_role(
    user_id: i64,
    role_id: i64,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let rows = UserRole::objects()
        .where_(UserRole::user_id.eq(user_id))
        .where_(UserRole::role_id.eq(role_id))
        .fetch(pool)
        .await?;
    for row in rows {
        row.delete_on(pool).await?;
    }
    Ok(())
}

/// Set a per-user permission override. Updates `granted` if a row already exists.
pub async fn set_user_perm(
    user_id: i64,
    codename: &str,
    granted: bool,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let existing = UserPermission::objects()
        .where_(UserPermission::user_id.eq(user_id))
        .where_(UserPermission::codename.eq(codename.to_owned()))
        .fetch(pool)
        .await?;
    if let Some(mut perm) = existing.into_iter().next() {
        perm.granted = granted;
        perm.save_on(pool).await?;
    } else {
        let mut perm = UserPermission {
            id: Auto::default(),
            user_id,
            codename: codename.to_owned(),
            granted,
        };
        perm.save_on(pool).await?;
    }
    Ok(())
}

/// Remove a per-user override, restoring role-based resolution.
pub async fn clear_user_perm(
    user_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let rows = UserPermission::objects()
        .where_(UserPermission::user_id.eq(user_id))
        .where_(UserPermission::codename.eq(codename.to_owned()))
        .fetch(pool)
        .await?;
    for row in rows {
        row.delete_on(pool).await?;
    }
    Ok(())
}

/// List all roles a user belongs to.
pub async fn user_roles_qs(user_id: i64, pool: &PgPool) -> Result<Vec<Role>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT r.id, r.name, r.description
           FROM   "rustango_roles" r
           JOIN   "rustango_user_roles" ur ON ur.role_id = r.id
           WHERE  ur.user_id = $1
           ORDER  BY r.name"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(Role {
                id: Auto::Set(row.try_get::<i64, _>("id")?),
                name: row.try_get("name")?,
                description: row.try_get("description")?,
            })
        })
        .collect()
}

/// List all `(role_id, name)` pairs for a user.
pub async fn user_roles(uid: i64, pool: &PgPool) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let roles = user_roles_qs(uid, pool).await?; // raw SQL join — stays sqlx::Error
    Ok(roles
        .into_iter()
        .map(|r| (r.id.get().copied().unwrap_or(0), r.name))
        .collect())
}

/// List all codenames a user has access to (union of role + direct grants,
/// minus denials). Superuser implicit grants are NOT included.
pub async fn user_permissions(uid: i64, pool: &PgPool) -> Result<Vec<String>, TenancyError> {
    let rows = sqlx::query(
        r#"
        SELECT codename FROM (
            SELECT rp.codename, TRUE AS granted
            FROM   "rustango_user_roles" ur
            JOIN   "rustango_role_permissions" rp ON rp.role_id = ur.role_id
            WHERE  ur.user_id = $1
            UNION
            SELECT codename, granted
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1
        ) combined
        WHERE granted = TRUE
        GROUP BY codename
        ORDER BY codename
        "#,
    )
    .bind(uid)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| r.try_get::<String, _>("codename").unwrap_or_default())
        .collect())
}

// ------------------------------------------------------------------ Codename helpers

/// Standard four codenames for a model table (`add`, `change`, `delete`, `view`).
#[must_use]
pub fn model_codenames(table: &str) -> [String; 4] {
    [
        format!("{table}.add"),
        format!("{table}.change"),
        format!("{table}.delete"),
        format!("{table}.view"),
    ]
}
