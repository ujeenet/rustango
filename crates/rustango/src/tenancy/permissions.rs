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

use crate::core::Model as _;
use crate::core::{ConflictClause, DeleteQuery, Filter, InsertQuery, Op, SqlValue, WhereExpr};
use crate::sql::sqlx::{self, PgPool, Row};
use crate::sql::Auto;
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
        list_display = "name, description",
        search_fields = "name, description",
        ordering = "name",
    )
)]
pub struct Role {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Human-readable name. Unique within the tenant.
    #[rustango(max_length = 150, unique)]
    pub name: String,
    #[rustango(max_length = 500)]
    pub description: String,
    /// Flexible role metadata — display config, feature flags, UI
    /// hints. Never read by the permission engine.
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
}

/// One codename granted to a role. Composite key (role_id, codename)
/// enforced by DB unique constraint; surrogate `id` for ORM compat.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_role_permissions",
    display = "codename",
    admin(
        list_display = "role_id, codename",
        search_fields = "codename",
        ordering = "role_id, codename",
    )
)]
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
#[rustango(
    table = "rustango_user_roles",
    admin(list_display = "user_id, role_id", ordering = "user_id, role_id",)
)]
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
#[rustango(
    table = "rustango_user_permissions",
    display = "codename",
    admin(
        list_display = "user_id, codename, granted",
        search_fields = "codename",
        ordering = "user_id, codename",
    )
)]
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
    /// Extra context on this override — reason, granted-by, expiry
    /// hints. Never read by `has_perm`.
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
}

// ------------------------------------------------------------------ ensure_tables (DDL)

const ENSURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_permissions" (
    "id"          BIGSERIAL    PRIMARY KEY,
    "table_name"  VARCHAR(150) NOT NULL,
    "codename"    VARCHAR(100) NOT NULL,
    "name"        VARCHAR(255) NOT NULL DEFAULT '',
    CONSTRAINT "rustango_permissions_uq" UNIQUE ("table_name", "codename")
);
CREATE TABLE IF NOT EXISTS "rustango_roles" (
    "id"          BIGSERIAL    PRIMARY KEY,
    "name"        VARCHAR(150) NOT NULL,
    "description" VARCHAR(500) NOT NULL DEFAULT '',
    "data"        JSONB        NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_roles_name_uq" UNIQUE ("name")
);
ALTER TABLE "rustango_roles"
    ADD COLUMN IF NOT EXISTS "data" JSONB NOT NULL DEFAULT '{}';
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
    "data"     JSONB        NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_user_permissions_uq" UNIQUE ("user_id", "codename")
);
ALTER TABLE "rustango_user_permissions"
    ADD COLUMN IF NOT EXISTS "data" JSONB NOT NULL DEFAULT '{}';
ALTER TABLE "rustango_users"
    ADD COLUMN IF NOT EXISTS "data" JSONB NOT NULL DEFAULT '{}';
ALTER TABLE "rustango_users"
    ADD COLUMN IF NOT EXISTS "password_changed_at" TIMESTAMPTZ NULL;
"#;

/// Ensure all four permission tables exist in `pool`'s schema.
/// Idempotent — safe to call on every boot. The tables are framework-
/// managed (like `rustango_audit_log`) and live outside the user's
/// migration chain; `make_migrations` sees them as baseline.
///
/// # Errors
/// Driver failures from `CREATE TABLE IF NOT EXISTS`.
pub async fn ensure_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    for stmt in ENSURE_SQL
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
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
    has_perm_on(uid, codename, pool).await
}

/// Like [`has_perm`] but accepts any sqlx executor. The
/// [`crate::viewset::ViewSet::tenant_router`] path uses this with the
/// per-request `&mut PgConnection` from
/// [`crate::extractors::Tenant::conn`] — `&PgPool` isn't usable in
/// schema-mode tenancy because each query needs `SET search_path`
/// against the same connection that issued it.
///
/// # Errors
/// As [`has_perm`].
pub async fn has_perm_on<'c, E>(uid: i64, codename: &str, executor: E) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
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
    .fetch_one(executor)
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
///
/// Single round-trip — resolves superuser, explicit denials, explicit
/// grants, and role-based grants in one CTE for the full array.
pub async fn has_any_perm(
    uid: i64,
    codenames: &[&str],
    pool: &PgPool,
) -> Result<bool, sqlx::Error> {
    if codenames.is_empty() {
        return Ok(false);
    }
    let names: Vec<&str> = codenames.to_vec();
    let row = sqlx::query(
        r#"
        WITH user_info AS (
            SELECT is_superuser
            FROM   "rustango_users"
            WHERE  id = $1 AND active = TRUE
        ),
        denied AS (
            SELECT codename
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1 AND granted = FALSE AND codename = ANY($2::text[])
        ),
        via_role AS (
            SELECT 1
            FROM   "rustango_user_roles" ur
            JOIN   "rustango_role_permissions" rp ON rp.role_id = ur.role_id
            WHERE  ur.user_id = $1
              AND  rp.codename = ANY($2::text[])
              AND  rp.codename NOT IN (SELECT codename FROM denied)
            LIMIT  1
        ),
        explicit_grant AS (
            SELECT 1
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1 AND granted = TRUE
              AND  codename = ANY($2::text[])
              AND  codename NOT IN (SELECT codename FROM denied)
            LIMIT  1
        )
        SELECT
            COALESCE((SELECT is_superuser FROM user_info), FALSE)                            AS is_super,
            EXISTS(SELECT 1 FROM via_role) OR EXISTS(SELECT 1 FROM explicit_grant) AS has_any
        "#,
    )
    .bind(uid)
    .bind(names)
    .fetch_one(pool)
    .await?;

    let is_super: bool = row.try_get("is_super").unwrap_or(false);
    if is_super {
        return Ok(true);
    }
    Ok(row.try_get("has_any").unwrap_or(false))
}

/// Check whether user `uid` holds ALL of the given `codenames`.
///
/// Single round-trip — counts effective grants (after applying denials)
/// and compares against the full codename list.
pub async fn has_all_perms(
    uid: i64,
    codenames: &[&str],
    pool: &PgPool,
) -> Result<bool, sqlx::Error> {
    if codenames.is_empty() {
        return Ok(true);
    }
    let names: Vec<&str> = codenames.to_vec();
    let expected = codenames.len() as i64;
    let row = sqlx::query(
        r#"
        WITH user_info AS (
            SELECT is_superuser
            FROM   "rustango_users"
            WHERE  id = $1 AND active = TRUE
        ),
        denied AS (
            SELECT codename
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1 AND granted = FALSE AND codename = ANY($2::text[])
        ),
        effective AS (
            SELECT rp.codename
            FROM   "rustango_user_roles" ur
            JOIN   "rustango_role_permissions" rp ON rp.role_id = ur.role_id
            WHERE  ur.user_id = $1
              AND  rp.codename = ANY($2::text[])
              AND  rp.codename NOT IN (SELECT codename FROM denied)

            UNION

            SELECT codename
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1 AND granted = TRUE
              AND  codename = ANY($2::text[])
              AND  codename NOT IN (SELECT codename FROM denied)
        )
        SELECT
            COALESCE((SELECT is_superuser FROM user_info), FALSE) AS is_super,
            COUNT(DISTINCT codename)                               AS matched
        FROM effective
        "#,
    )
    .bind(uid)
    .bind(names)
    .fetch_one(pool)
    .await?;

    let is_super: bool = row.try_get("is_super").unwrap_or(false);
    if is_super {
        return Ok(true);
    }
    let matched: i64 = row.try_get("matched").unwrap_or(0);
    Ok(matched == expected)
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
        data: serde_json::Value::Object(serde_json::Map::new()),
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
    // Single round-trip: insert if absent, then union-select the id.
    let row = sqlx::query(
        r#"
        WITH ins AS (
            INSERT INTO "rustango_roles" (name, description, data)
            VALUES ($1, $2, '{}')
            ON CONFLICT (name) DO NOTHING
            RETURNING id
        )
        SELECT id FROM ins
        UNION ALL
        SELECT id FROM "rustango_roles" WHERE name = $1
        LIMIT 1
        "#,
    )
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get::<i64, _>("id").unwrap_or(0))
}

/// Grant a codename to a role. No-op if already granted.
///
/// Routed through the ORM's [`InsertQuery`] IR with
/// [`ConflictClause::DoNothing`] — the writer emits `INSERT … ON
/// CONFLICT DO NOTHING`, which matches the `(role_id, codename)`
/// unique constraint declared in [`ENSURE_SQL`].
pub async fn grant_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let query = InsertQuery {
        model: RolePermission::SCHEMA,
        columns: vec!["role_id", "codename"],
        values: vec![SqlValue::from(role_id), SqlValue::from(codename.to_owned())],
        returning: vec![],
        on_conflict: Some(ConflictClause::DoNothing),
    };
    crate::sql::insert(pool, &query).await?;
    Ok(())
}

/// Revoke a codename from a role.
pub async fn revoke_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    crate::sql::delete(
        pool,
        &DeleteQuery {
            model: RolePermission::SCHEMA,
            where_clause: WhereExpr::and_predicates(vec![
                Filter {
                    column: "role_id",
                    op: Op::Eq,
                    value: SqlValue::from(role_id),
                },
                Filter {
                    column: "codename",
                    op: Op::Eq,
                    value: SqlValue::from(codename),
                },
            ]),
        },
    )
    .await?;
    Ok(())
}

/// Assign a user to a role. No-op if already assigned.
///
/// Same IR-routed pattern as [`grant_role_perm`].
pub async fn assign_role(user_id: i64, role_id: i64, pool: &PgPool) -> Result<(), TenancyError> {
    let query = InsertQuery {
        model: UserRole::SCHEMA,
        columns: vec!["user_id", "role_id"],
        values: vec![SqlValue::from(user_id), SqlValue::from(role_id)],
        returning: vec![],
        on_conflict: Some(ConflictClause::DoNothing),
    };
    crate::sql::insert(pool, &query).await?;
    Ok(())
}

/// Remove a user from a role.
pub async fn remove_role(user_id: i64, role_id: i64, pool: &PgPool) -> Result<(), TenancyError> {
    crate::sql::delete(
        pool,
        &DeleteQuery {
            model: UserRole::SCHEMA,
            where_clause: WhereExpr::and_predicates(vec![
                Filter {
                    column: "user_id",
                    op: Op::Eq,
                    value: SqlValue::from(user_id),
                },
                Filter {
                    column: "role_id",
                    op: Op::Eq,
                    value: SqlValue::from(role_id),
                },
            ]),
        },
    )
    .await?;
    Ok(())
}

/// Set a per-user permission override. Updates `granted` if a row already exists.
///
/// Routed through the ORM's [`InsertQuery`] IR with
/// [`ConflictClause::DoUpdate`] — the writer emits `INSERT … ON
/// CONFLICT (user_id, codename) DO UPDATE SET granted = EXCLUDED.granted`,
/// matching the composite unique constraint in [`ENSURE_SQL`]. `data`
/// is omitted from `update_columns` so the existing JSONB context
/// (reason / granted-by / etc.) survives a re-grant.
pub async fn set_user_perm(
    user_id: i64,
    codename: &str,
    granted: bool,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    let query = InsertQuery {
        model: UserPermission::SCHEMA,
        columns: vec!["user_id", "codename", "granted", "data"],
        values: vec![
            SqlValue::from(user_id),
            SqlValue::from(codename.to_owned()),
            SqlValue::from(granted),
            SqlValue::Json(serde_json::json!({})),
        ],
        returning: vec![],
        on_conflict: Some(ConflictClause::DoUpdate {
            target: vec!["user_id", "codename"],
            update_columns: vec!["granted"],
        }),
    };
    crate::sql::insert(pool, &query).await?;
    Ok(())
}

/// Remove a per-user override, restoring role-based resolution.
pub async fn clear_user_perm(
    user_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    crate::sql::delete(
        pool,
        &DeleteQuery {
            model: UserPermission::SCHEMA,
            where_clause: WhereExpr::and_predicates(vec![
                Filter {
                    column: "user_id",
                    op: Op::Eq,
                    value: SqlValue::from(user_id),
                },
                Filter {
                    column: "codename",
                    op: Op::Eq,
                    value: SqlValue::from(codename),
                },
            ]),
        },
    )
    .await?;
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
                data: row
                    .try_get::<serde_json::Value, _>("data")
                    .unwrap_or_else(|_| serde_json::json!({})),
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
/// minus explicit denials). Superuser implicit grants are NOT included —
/// callers that need to handle superusers should check `is_superuser` first.
///
/// Denial priority matches [`has_perm`]: an explicit `granted = false` row
/// removes the codename even if a role would otherwise grant it.
pub async fn user_permissions(uid: i64, pool: &PgPool) -> Result<Vec<String>, TenancyError> {
    let rows = sqlx::query(
        r#"
        WITH denied AS (
            SELECT codename
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1 AND granted = FALSE
        )
        SELECT DISTINCT codename
        FROM (
            SELECT rp.codename
            FROM   "rustango_user_roles" ur
            JOIN   "rustango_role_permissions" rp ON rp.role_id = ur.role_id
            WHERE  ur.user_id = $1
              AND  rp.codename NOT IN (SELECT codename FROM denied)

            UNION ALL

            SELECT codename
            FROM   "rustango_user_permissions"
            WHERE  user_id = $1 AND granted = TRUE
              AND  codename NOT IN (SELECT codename FROM denied)
        ) effective
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

/// Seed the `rustango_permissions` catalog with the four standard CRUD
/// codenames for every model that carries `#[rustango(permissions)]`.
///
/// Idempotent — uses `ON CONFLICT DO NOTHING`. Call once at startup after
/// [`ensure_tables`] so the catalog reflects the current model set.
///
/// # Errors
/// Driver / SQL failures.
pub async fn auto_create_permissions(pool: &PgPool) -> Result<(), sqlx::Error> {
    use crate::core::{inventory, ModelEntry};

    let action_names = [
        ("add", "Can add"),
        ("change", "Can change"),
        ("delete", "Can delete"),
        ("view", "Can view"),
    ];

    let mut tables: Vec<&str> = Vec::new();
    let mut codenames: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();

    for entry in inventory::iter::<ModelEntry> {
        if !entry.schema.permissions {
            continue;
        }
        let table = entry.schema.table;
        let model_name = entry.schema.name;
        for (action, verb) in &action_names {
            tables.push(table);
            codenames.push(format!("{table}.{action}"));
            names.push(format!("{verb} {model_name}"));
        }
    }

    if tables.is_empty() {
        return Ok(());
    }

    sqlx::query(
        r#"INSERT INTO "rustango_permissions" (table_name, codename, name)
           SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[])
           ON CONFLICT (table_name, codename) DO NOTHING"#,
    )
    .bind(&tables)
    .bind(&codenames)
    .bind(&names)
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod admin_config_tests {
    use super::*;
    use crate::core::Model;

    /// v0.28 — every model in this module participates in the auto-admin.
    /// Without admin config, list views render every column raw — usable
    /// but noisy. The configs below pick sensible `list_display` so
    /// operators see role/user IDs + codenames at a glance.
    #[test]
    fn perm_models_carry_admin_config() {
        for (label, schema) in [
            ("Role", Role::SCHEMA),
            ("RolePermission", RolePermission::SCHEMA),
            ("UserRole", UserRole::SCHEMA),
            ("UserPermission", UserPermission::SCHEMA),
        ] {
            assert!(schema.admin.is_some(), "expected admin config on {label}");
        }
    }

    #[test]
    fn perm_models_keep_tenant_scope() {
        // None of these are registry-scoped — they live in each
        // tenant's storage. The scope filter on the admin sidebar
        // must therefore include them when the admin is mounted in
        // tenant mode (which it is by default in `server::Builder`).
        for schema in [
            Role::SCHEMA,
            RolePermission::SCHEMA,
            UserRole::SCHEMA,
            UserPermission::SCHEMA,
        ] {
            assert_eq!(schema.scope, crate::core::ModelScope::Tenant);
        }
    }
}
