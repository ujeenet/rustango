//! Role-based permission engine for rustango tenants.
//!
//! ## Schema (per-tenant tables, created via [`ensure_tables`])
//!
//! ```text
//! rustango_roles              id, name, description
//! rustango_role_permissions   role_id, codename
//! rustango_user_roles         user_id, role_id
//! rustango_user_permissions   user_id, codename, granted
//! ```
//!
//! ## Codename convention
//!
//! `{model_table}.{action}` — e.g. `post.add`, `post.change`,
//! `post.delete`, `post.view`. Superusers pass every check.
//!
//! ## Effective permission resolution
//!
//! `has_perm(uid, codename, pool)`:
//! 1. Superuser short-circuit → true.
//! 2. Explicit per-user denial (`granted = false`) → false.
//! 3. Explicit per-user grant (`granted = true`) → true.
//! 4. Any role the user belongs to grants `codename` → true.
//! 5. Default → false.

use crate::sql::sqlx::{self, PgPool, Row};

use super::error::TenancyError;

// ------------------------------------------------------------------ DDL

const ENSURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_roles" (
    "id"          BIGSERIAL PRIMARY KEY,
    "name"        VARCHAR(150) NOT NULL,
    "description" VARCHAR(500) NOT NULL DEFAULT '',
    CONSTRAINT "rustango_roles_name_uq" UNIQUE ("name")
);
CREATE TABLE IF NOT EXISTS "rustango_role_permissions" (
    "role_id"  BIGINT      NOT NULL
                            REFERENCES "rustango_roles"("id")
                            ON DELETE CASCADE,
    "codename" VARCHAR(100) NOT NULL,
    PRIMARY KEY ("role_id", "codename")
);
CREATE TABLE IF NOT EXISTS "rustango_user_roles" (
    "user_id" BIGINT NOT NULL
                     REFERENCES "rustango_users"("id")
                     ON DELETE CASCADE,
    "role_id" BIGINT NOT NULL
                     REFERENCES "rustango_roles"("id")
                     ON DELETE CASCADE,
    PRIMARY KEY ("user_id", "role_id")
);
CREATE TABLE IF NOT EXISTS "rustango_user_permissions" (
    "user_id"  BIGINT       NOT NULL
                             REFERENCES "rustango_users"("id")
                             ON DELETE CASCADE,
    "codename" VARCHAR(100)  NOT NULL,
    "granted"  BOOLEAN       NOT NULL DEFAULT TRUE,
    PRIMARY KEY ("user_id", "codename")
);
"#;

/// Ensure all four permission tables exist in `pool`'s schema.
/// Idempotent — safe to call on every boot.
///
/// # Errors
/// Driver failures from `CREATE TABLE IF NOT EXISTS`.
pub async fn ensure_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    for stmt in ENSURE_SQL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

// ------------------------------------------------------------------ Check

/// Check whether user `uid` holds permission `codename` in `pool`.
///
/// Resolution order:
/// 1. Superuser → always `true`.
/// 2. Explicit per-user denial (`granted = false`) → `false`.
/// 3. Explicit per-user grant (`granted = true`) → `true`.
/// 4. Any role the user belongs to grants `codename` → `true`.
/// 5. Default → `false`.
///
/// One round-trip using a CTE.
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
    let via_role: bool = row.try_get("via_role").unwrap_or(false);
    Ok(via_role)
}

/// Check whether user `uid` holds ANY of the given `codenames`.
///
/// # Errors
/// As [`has_perm`].
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
///
/// # Errors
/// As [`has_perm`].
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

// ------------------------------------------------------------------ Role management

/// Create a role, returning its id. Errors if name already exists.
///
/// # Errors
/// Driver errors or unique-constraint violation.
pub async fn create_role(name: &str, description: &str, pool: &PgPool) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        r#"INSERT INTO "rustango_roles" (name, description)
           VALUES ($1, $2) RETURNING id"#,
    )
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await?;
    row.try_get("id")
}

/// Create a role if one with `name` doesn't already exist. Returns the id.
///
/// # Errors
/// Driver errors.
pub async fn get_or_create_role(
    name: &str,
    description: &str,
    pool: &PgPool,
) -> Result<i64, sqlx::Error> {
    let existing = sqlx::query(r#"SELECT id FROM "rustango_roles" WHERE name = $1"#)
        .bind(name)
        .fetch_optional(pool)
        .await?;
    if let Some(row) = existing {
        return row.try_get("id");
    }
    create_role(name, description, pool).await
}

/// Grant a codename permission to a role.
///
/// # Errors
/// Driver errors.
pub async fn grant_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO "rustango_role_permissions" (role_id, codename)
           VALUES ($1, $2)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(role_id)
    .bind(codename)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revoke a codename permission from a role.
///
/// # Errors
/// Driver errors.
pub async fn revoke_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"DELETE FROM "rustango_role_permissions"
           WHERE role_id = $1 AND codename = $2"#,
    )
    .bind(role_id)
    .bind(codename)
    .execute(pool)
    .await?;
    Ok(())
}

/// Assign a user to a role.
///
/// # Errors
/// Driver errors.
pub async fn assign_role(
    user_id: i64,
    role_id: i64,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO "rustango_user_roles" (user_id, role_id)
           VALUES ($1, $2)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(user_id)
    .bind(role_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a user from a role.
///
/// # Errors
/// Driver errors.
pub async fn remove_role(
    user_id: i64,
    role_id: i64,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"DELETE FROM "rustango_user_roles"
           WHERE user_id = $1 AND role_id = $2"#,
    )
    .bind(user_id)
    .bind(role_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set a per-user permission override.
///
/// # Errors
/// Driver errors.
pub async fn set_user_perm(
    user_id: i64,
    codename: &str,
    granted: bool,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO "rustango_user_permissions" (user_id, codename, granted)
           VALUES ($1, $2, $3)
           ON CONFLICT (user_id, codename) DO UPDATE SET granted = EXCLUDED.granted"#,
    )
    .bind(user_id)
    .bind(codename)
    .bind(granted)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a per-user permission override, restoring role-based resolution.
///
/// # Errors
/// Driver errors.
pub async fn clear_user_perm(
    user_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"DELETE FROM "rustango_user_permissions"
           WHERE user_id = $1 AND codename = $2"#,
    )
    .bind(user_id)
    .bind(codename)
    .execute(pool)
    .await?;
    Ok(())
}

/// List all roles for a user (`(role_id, name)` pairs).
///
/// # Errors
/// Driver errors.
pub async fn user_roles(
    user_id: i64,
    pool: &PgPool,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT r.id, r.name
           FROM   "rustango_roles" r
           JOIN   "rustango_user_roles" ur ON ur.role_id = r.id
           WHERE  ur.user_id = $1
           ORDER  BY r.name"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| Ok((r.try_get::<i64, _>("id")?, r.try_get::<String, _>("name")?)))
        .collect()
}

/// List all effective codenames for a user (union of role + direct grants,
/// minus explicit denials). Does NOT include superuser implicit grants.
///
/// # Errors
/// Driver errors.
pub async fn user_permissions(
    user_id: i64,
    pool: &PgPool,
) -> Result<Vec<String>, TenancyError> {
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
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| r.try_get::<String, _>("codename").unwrap_or_default()).collect())
}

// ------------------------------------------------------------------ Codename helpers

/// Standard codenames for a model table — `add`, `change`, `delete`, `view`.
#[must_use]
pub fn model_codenames(table: &str) -> [String; 4] {
    [
        format!("{table}.add"),
        format!("{table}.change"),
        format!("{table}.delete"),
        format!("{table}.view"),
    ]
}
