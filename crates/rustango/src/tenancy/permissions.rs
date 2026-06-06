//! Role-based permission engine for rustango tenants.
//!
//! All five tables are proper [`Model`] types — you can use the full
//! queryset ORM, the auto-admin, and `make_migrations` sees them as
//! baseline (they live in the bootstrap snapshot, not user migrations).
//!
//! ## Tables (per-tenant, created via [`ensure_tables_pool`])
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
use crate::sql::sqlx;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::{PgPool, Row};
use crate::sql::Auto;
use crate::Model;

use super::error::TenancyError;

// ------------------------------------------------------------------ Reserved codenames (issue #311)

/// Reserved permission codename gating the framework's auto-admin
/// surface (`/__admin/`, `/admin/`). Issue #311.
///
/// Granting this codename to a user (or to a role they belong to)
/// lets them load the framework admin without `is_superuser = true`.
/// Superusers bypass this check (and every other codename check) —
/// the bypass is baked into [`has_perm_pool`].
///
/// Downstream consuming crates (`rustango-cms`, etc.) declare their
/// own admin codenames following the same convention:
/// `auth.access_<crate>_admin`. They seed the row at their own
/// migration time and check it with
/// [`crate::auth_decorators::permission_required`].
pub const ACCESS_ADMIN_CODENAME: &str = "auth.access_admin";

/// Logical "table" prefix used when seeding non-model-bound codenames
/// into `rustango_permissions`. The auto-admin model-CRUD codenames
/// use the model's actual table name; reserved cross-cutting
/// codenames like [`ACCESS_ADMIN_CODENAME`] sit under the `auth`
/// namespace.
const AUTH_NAMESPACE: &str = "auth";

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
    #[rustango(fk = "rustango_roles", on = "id", on_delete = "cascade")]
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
    #[rustango(fk = "rustango_users", on = "id", on_delete = "cascade")]
    pub user_id: i64,
    /// `rustango_roles.id`
    #[rustango(fk = "rustango_roles", on = "id", on_delete = "cascade")]
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
    #[rustango(fk = "rustango_users", on = "id", on_delete = "cascade")]
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

// ------------------------------------------------------------------ ensure_tables_pool (DDL)

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

/// v0.38 — SQLite counterpart of [`ENSURE_SQL`].
///
/// Differences from the PG version:
/// - `BIGSERIAL` → `INTEGER PRIMARY KEY AUTOINCREMENT` (SQLite's
///   auto-rowid spelling — `INTEGER PRIMARY KEY` aliases to the
///   internal `ROWID`, AUTOINCREMENT enforces monotonic non-reuse).
/// - `BIGINT` / `VARCHAR(N)` / `JSONB` / `BOOLEAN` / `TIMESTAMPTZ`
///   collapse to SQLite's loose affinities (`INTEGER` / `TEXT`).
/// - `DEFAULT TRUE` → `DEFAULT 1` (SQLite has no bool literal —
///   1/0 are the canonical encoding the sqlx-sqlite driver reads
///   back via `<bool as Decode<Sqlite>>`).
/// - No `ALTER TABLE ADD COLUMN IF NOT EXISTS`. The PG ALTER block
///   is a legacy backfill for ≤0.27 installs; fresh SQLite tenants
///   never need it because the columns ship in the CREATE TABLE.
const ENSURE_SQL_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_permissions" (
    "id"          INTEGER     PRIMARY KEY AUTOINCREMENT,
    "table_name"  TEXT        NOT NULL,
    "codename"    TEXT        NOT NULL,
    "name"        TEXT        NOT NULL DEFAULT '',
    CONSTRAINT "rustango_permissions_uq" UNIQUE ("table_name", "codename")
);
CREATE TABLE IF NOT EXISTS "rustango_roles" (
    "id"          INTEGER     PRIMARY KEY AUTOINCREMENT,
    "name"        TEXT        NOT NULL,
    "description" TEXT        NOT NULL DEFAULT '',
    "data"        TEXT        NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_roles_name_uq" UNIQUE ("name")
);
CREATE TABLE IF NOT EXISTS "rustango_role_permissions" (
    "id"       INTEGER  PRIMARY KEY AUTOINCREMENT,
    "role_id"  INTEGER  NOT NULL
                         REFERENCES "rustango_roles"("id")
                         ON DELETE CASCADE,
    "codename" TEXT     NOT NULL,
    CONSTRAINT "rustango_role_permissions_uq" UNIQUE ("role_id", "codename")
);
CREATE TABLE IF NOT EXISTS "rustango_user_roles" (
    "id"      INTEGER PRIMARY KEY AUTOINCREMENT,
    "user_id" INTEGER NOT NULL
                       REFERENCES "rustango_users"("id")
                       ON DELETE CASCADE,
    "role_id" INTEGER NOT NULL
                       REFERENCES "rustango_roles"("id")
                       ON DELETE CASCADE,
    CONSTRAINT "rustango_user_roles_uq" UNIQUE ("user_id", "role_id")
);
CREATE TABLE IF NOT EXISTS "rustango_user_permissions" (
    "id"       INTEGER  PRIMARY KEY AUTOINCREMENT,
    "user_id"  INTEGER  NOT NULL
                         REFERENCES "rustango_users"("id")
                         ON DELETE CASCADE,
    "codename" TEXT     NOT NULL,
    "granted"  INTEGER  NOT NULL DEFAULT 1,
    "data"     TEXT     NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_user_permissions_uq" UNIQUE ("user_id", "codename")
);
"#;

/// v0.38 — MySQL counterpart of [`ENSURE_SQL`]. Identifier quoting
/// is backticks (MySQL rejects double-quoted identifiers in default
/// `ANSI_QUOTES=off` mode). `BIGSERIAL` → `BIGINT AUTO_INCREMENT
/// PRIMARY KEY`. `JSONB` → `JSON`. `TIMESTAMPTZ` → `DATETIME(6)`.
/// `BOOLEAN` is a `TINYINT(1)` alias. Same legacy-ALTER posture as
/// SQLite: omit the back-compat columns — fresh MySQL tenants ship
/// the `data` / `password_changed_at` columns from the CREATE.
const ENSURE_SQL_MYSQL: &str = r#"
CREATE TABLE IF NOT EXISTS `rustango_permissions` (
    `id`          BIGINT AUTO_INCREMENT PRIMARY KEY,
    `table_name`  VARCHAR(150) NOT NULL,
    `codename`    VARCHAR(100) NOT NULL,
    `name`        VARCHAR(255) NOT NULL DEFAULT '',
    CONSTRAINT `rustango_permissions_uq` UNIQUE (`table_name`, `codename`)
);
CREATE TABLE IF NOT EXISTS `rustango_roles` (
    `id`          BIGINT AUTO_INCREMENT PRIMARY KEY,
    `name`        VARCHAR(150) NOT NULL,
    `description` VARCHAR(500) NOT NULL DEFAULT '',
    `data`        JSON         NOT NULL,
    CONSTRAINT `rustango_roles_name_uq` UNIQUE (`name`)
);
CREATE TABLE IF NOT EXISTS `rustango_role_permissions` (
    `id`       BIGINT AUTO_INCREMENT PRIMARY KEY,
    `role_id`  BIGINT       NOT NULL,
    `codename` VARCHAR(100) NOT NULL,
    CONSTRAINT `rustango_role_permissions_uq` UNIQUE (`role_id`, `codename`),
    CONSTRAINT `rustango_role_permissions_fk_role`
        FOREIGN KEY (`role_id`) REFERENCES `rustango_roles`(`id`) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS `rustango_user_roles` (
    `id`      BIGINT AUTO_INCREMENT PRIMARY KEY,
    `user_id` BIGINT NOT NULL,
    `role_id` BIGINT NOT NULL,
    CONSTRAINT `rustango_user_roles_uq` UNIQUE (`user_id`, `role_id`),
    CONSTRAINT `rustango_user_roles_fk_user`
        FOREIGN KEY (`user_id`) REFERENCES `rustango_users`(`id`) ON DELETE CASCADE,
    CONSTRAINT `rustango_user_roles_fk_role`
        FOREIGN KEY (`role_id`) REFERENCES `rustango_roles`(`id`) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS `rustango_user_permissions` (
    `id`       BIGINT AUTO_INCREMENT PRIMARY KEY,
    `user_id`  BIGINT       NOT NULL,
    `codename` VARCHAR(100) NOT NULL,
    `granted`  BOOLEAN      NOT NULL DEFAULT TRUE,
    `data`     JSON         NOT NULL,
    CONSTRAINT `rustango_user_permissions_uq` UNIQUE (`user_id`, `codename`),
    CONSTRAINT `rustango_user_permissions_fk_user`
        FOREIGN KEY (`user_id`) REFERENCES `rustango_users`(`id`) ON DELETE CASCADE
);
"#;

/// Ensure all four permission tables exist in `pool`'s schema.
/// Idempotent — safe to call on every boot. Picks the per-dialect
/// DDL constant ([`ENSURE_SQL`] / [`ENSURE_SQL_SQLITE`] /
/// [`ENSURE_SQL_MYSQL`]) and runs each statement as its own round-trip
/// because sqlx's simple-prepare path rejects multi-statement strings.
///
/// Long-term, the bootstrap tenant migration should create every
/// table the engine needs and this runtime helper goes away; today
/// the bootstrap only emits `CreateTable rustango_users` (the seven
/// auth/perm tables live in its snapshot but never get DDL'd by the
/// migration runner). This helper plugs that gap on all three
/// dialects.
///
/// # Errors
/// Driver / SQL failures from any `CREATE TABLE IF NOT EXISTS`.
pub async fn ensure_tables_pool(pool: &crate::sql::Pool) -> Result<(), sqlx::Error> {
    let dialect = pool.dialect();
    let ddl = match dialect.name() {
        "sqlite" => ENSURE_SQL_SQLITE,
        "mysql" => ENSURE_SQL_MYSQL,
        // PG and any future dialect fall through to the original
        // BIGSERIAL/JSONB/TIMESTAMPTZ DDL.
        _ => ENSURE_SQL,
    };
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        crate::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .map_err(|e| match e {
                crate::sql::ExecError::Driver(err) => err,
                other => sqlx::Error::Protocol(format!("{other}")),
            })?;
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
#[cfg(feature = "postgres")]
pub async fn has_perm(uid: i64, codename: &str, pool: &PgPool) -> Result<bool, sqlx::Error> {
    has_perm_on(uid, codename, pool).await
}

/// v0.38 — tri-dialect counterpart of [`has_perm`]. Trades the single
/// CTE round-trip for three ORM queries (user-info, explicit grant,
/// role-via grant) — each an indexed lookup, total cost ≈ 3× the
/// CTE in latency but portable across PG/MySQL/SQLite without
/// dialect-specific `TRUE`/`FALSE` literals or array binding.
///
/// Resolution order is identical to [`has_perm`]:
/// 1. Superuser → true
/// 2. Explicit per-user denial (`granted = false`) → false
/// 3. Explicit per-user grant (`granted = true`) → true
/// 4. Any role the user belongs to grants `codename` → true
/// 5. Default → false
///
/// # Errors
/// Driver / SQL failures.
pub async fn has_perm_pool(
    uid: i64,
    codename: &str,
    pool: &crate::sql::Pool,
) -> Result<bool, TenancyError> {
    use sqlx::Row as _;
    let dialect = pool.dialect();
    let users_t = dialect.quote_ident("rustango_users");
    let user_perms_t = dialect.quote_ident("rustango_user_permissions");
    let user_roles_t = dialect.quote_ident("rustango_user_roles");
    let role_perms_t = dialect.quote_ident("rustango_role_permissions");
    // v0.38 — bind uid 3 times + codename 2 times. PG `placeholder(n)`
    // generates `$n` which the driver reuses, but MySQL/SQLite use
    // positional `?` so each occurrence must be a distinct bind.
    // Emitting 5 distinct placeholders + binding 5 values is portable
    // across all three.
    let p_uid_a = dialect.placeholder(1);
    let p_uid_b = dialect.placeholder(2);
    let p_cn_a = dialect.placeholder(3);
    let p_uid_c = dialect.placeholder(4);
    let p_cn_b = dialect.placeholder(5);
    let true_lit = dialect.bool_literal(true);
    let false_lit = dialect.bool_literal(false);
    let sql = format!(
        "SELECT \
            COALESCE((SELECT is_superuser FROM {users_t} \
                      WHERE id = {p_uid_a} AND active = {true_lit}), {false_lit}) AS is_super, \
            (SELECT granted FROM {user_perms_t} \
                WHERE user_id = {p_uid_b} AND codename = {p_cn_a}) AS explicit_grant, \
            EXISTS(SELECT 1 FROM {user_roles_t} ur \
                   JOIN {role_perms_t} rp ON rp.role_id = ur.role_id \
                   WHERE ur.user_id = {p_uid_c} AND rp.codename = {p_cn_b}) AS via_role"
    );
    let (is_super, explicit_grant, via_role) = match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let row = sqlx::query(&sql)
                .bind(uid)
                .bind(uid)
                .bind(codename)
                .bind(uid)
                .bind(codename)
                .fetch_one(pg)
                .await?;
            (
                row.try_get::<bool, _>("is_super").unwrap_or(false),
                row.try_get::<Option<bool>, _>("explicit_grant")
                    .unwrap_or(None),
                row.try_get::<bool, _>("via_role").unwrap_or(false),
            )
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let row = sqlx::query(&sql)
                .bind(uid)
                .bind(uid)
                .bind(codename)
                .bind(uid)
                .bind(codename)
                .fetch_one(my)
                .await?;
            // MySQL's EXISTS returns 0/1 as i64, BOOLEAN is TINYINT(1).
            let is_super: i64 = row.try_get("is_super").unwrap_or(0);
            let explicit: Option<i64> = row.try_get("explicit_grant").unwrap_or(None);
            let via_role: i64 = row.try_get("via_role").unwrap_or(0);
            (is_super != 0, explicit.map(|v| v != 0), via_role != 0)
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let row = sqlx::query(&sql)
                .bind(uid)
                .bind(uid)
                .bind(codename)
                .bind(uid)
                .bind(codename)
                .fetch_one(sq)
                .await?;
            // SQLite bools come back as i64; explicit_grant nullable.
            let is_super: i64 = row.try_get("is_super").unwrap_or(0);
            let explicit: Option<i64> = row.try_get("explicit_grant").unwrap_or(None);
            let via_role: i64 = row.try_get("via_role").unwrap_or(0);
            (is_super != 0, explicit.map(|v| v != 0), via_role != 0)
        }
    };
    if is_super {
        return Ok(true);
    }
    if let Some(granted) = explicit_grant {
        return Ok(granted);
    }
    Ok(via_role)
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
#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
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
///
/// #562 — delegates to [`create_role_pool`] so the Insert IR
/// builder lives in one place. The PG-typed signature stays for
/// back-compat with existing call sites.
#[cfg(feature = "postgres")]
pub async fn create_role(
    name: &str,
    description: &str,
    pool: &PgPool,
) -> Result<i64, TenancyError> {
    create_role_pool(name, description, &crate::sql::Pool::from(pool.clone())).await
}

/// v0.38 — tri-dialect counterpart of [`create_role`].
///
/// # Errors
/// As [`create_role`].
pub async fn create_role_pool(
    name: &str,
    description: &str,
    pool: &crate::sql::Pool,
) -> Result<i64, TenancyError> {
    let mut role = Role {
        id: Auto::default(),
        name: name.to_owned(),
        description: description.to_owned(),
        data: serde_json::Value::Object(serde_json::Map::new()),
    };
    role.save_pool(pool).await?;
    Ok(role.id.get().copied().unwrap_or(0))
}

/// v0.38 — tri-dialect counterpart of [`get_or_create_role`]. Two
/// round-trips on a fresh insert (SELECT-by-name, then INSERT on miss)
/// vs. the single-CTE PG variant; the bound surface is small and
/// idempotency is preserved by the unique constraint on `name`.
///
/// # Errors
/// As [`get_or_create_role`].
pub async fn get_or_create_role_pool(
    name: &str,
    description: &str,
    pool: &crate::sql::Pool,
) -> Result<i64, TenancyError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    let existing: Vec<Role> = Role::objects()
        .where_(Role::name.eq(name.to_owned()))
        .limit(1)
        .fetch_pool(pool)
        .await?;
    if let Some(r) = existing.into_iter().next() {
        return Ok(r.id.get().copied().unwrap_or(0));
    }
    create_role_pool(name, description, pool).await
}

/// v0.38 — tri-dialect counterpart of [`has_any_perm`]. Issues one
/// `has_perm_pool` per codename and short-circuits on the first hit.
/// PG keeps the single-CTE [`has_any_perm`] for hot paths; the
/// loop-per-codename path is fine for sqlite/mysql given the typical
/// codename-list size (≤ a handful).
///
/// # Errors
/// As [`has_perm_pool`].
pub async fn has_any_perm_pool(
    uid: i64,
    codenames: &[&str],
    pool: &crate::sql::Pool,
) -> Result<bool, TenancyError> {
    for c in codenames {
        if has_perm_pool(uid, c, pool).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// v0.38 — tri-dialect counterpart of [`has_all_perms`]. Issues one
/// `has_perm_pool` per codename and short-circuits on the first miss.
///
/// # Errors
/// As [`has_perm_pool`].
pub async fn has_all_perms_pool(
    uid: i64,
    codenames: &[&str],
    pool: &crate::sql::Pool,
) -> Result<bool, TenancyError> {
    for c in codenames {
        if !has_perm_pool(uid, c, pool).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// v0.38 — tri-dialect counterpart of [`user_roles`]. Reuses
/// [`user_roles_qs_pool`] and projects to `(id, name)` pairs.
///
/// # Errors
/// As [`user_roles_qs_pool`].
pub async fn user_roles_pool(
    uid: i64,
    pool: &crate::sql::Pool,
) -> Result<Vec<(i64, String)>, TenancyError> {
    let roles = user_roles_qs_pool(uid, pool).await?;
    Ok(roles
        .into_iter()
        .map(|r| (r.id.get().copied().unwrap_or(0), r.name))
        .collect())
}

/// Get an existing role by name or create one. Returns the id.
#[cfg(feature = "postgres")]
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
/// #562 — delegates to [`grant_role_perm_pool`].
#[cfg(feature = "postgres")]
pub async fn grant_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    grant_role_perm_pool(role_id, codename, &crate::sql::Pool::from(pool.clone())).await
}

/// v0.38 — tri-dialect counterpart of [`grant_role_perm`].
///
/// # Errors
/// As [`grant_role_perm`].
pub async fn grant_role_perm_pool(
    role_id: i64,
    codename: &str,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    let query = InsertQuery {
        model: RolePermission::SCHEMA,
        columns: vec!["role_id", "codename"],
        values: vec![SqlValue::from(role_id), SqlValue::from(codename.to_owned())],
        returning: vec![],
        on_conflict: Some(ConflictClause::DoNothing),
    };
    crate::sql::insert_pool(pool, &query).await?;
    Ok(())
}

/// Revoke a codename from a role.
///
/// #562 — delegates to [`revoke_role_perm_pool`].
#[cfg(feature = "postgres")]
pub async fn revoke_role_perm(
    role_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    revoke_role_perm_pool(role_id, codename, &crate::sql::Pool::from(pool.clone())).await
}

/// v0.38 — tri-dialect counterpart of [`revoke_role_perm`].
///
/// # Errors
/// As [`revoke_role_perm`].
pub async fn revoke_role_perm_pool(
    role_id: i64,
    codename: &str,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    crate::sql::delete_pool(
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
/// #562 — delegates to [`assign_role_pool`].
#[cfg(feature = "postgres")]
pub async fn assign_role(user_id: i64, role_id: i64, pool: &PgPool) -> Result<(), TenancyError> {
    assign_role_pool(user_id, role_id, &crate::sql::Pool::from(pool.clone())).await
}

/// v0.38 — tri-dialect counterpart of [`assign_role`].
pub async fn assign_role_pool(
    user_id: i64,
    role_id: i64,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    let query = InsertQuery {
        model: UserRole::SCHEMA,
        columns: vec!["user_id", "role_id"],
        values: vec![SqlValue::from(user_id), SqlValue::from(role_id)],
        returning: vec![],
        on_conflict: Some(ConflictClause::DoNothing),
    };
    crate::sql::insert_pool(pool, &query).await?;
    Ok(())
}

/// Remove a user from a role.
///
/// #562 — delegates to [`remove_role_pool`].
#[cfg(feature = "postgres")]
pub async fn remove_role(user_id: i64, role_id: i64, pool: &PgPool) -> Result<(), TenancyError> {
    remove_role_pool(user_id, role_id, &crate::sql::Pool::from(pool.clone())).await
}

/// v0.38 — tri-dialect counterpart of [`remove_role`].
pub async fn remove_role_pool(
    user_id: i64,
    role_id: i64,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    crate::sql::delete_pool(
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
/// #562 — delegates to [`set_user_perm_pool`]. The
/// `InsertQuery` IR (with `ConflictClause::DoUpdate` targeting the
/// `(user_id, codename)` unique constraint from [`ENSURE_SQL`]) lives
/// there; the `granted` column is the only one in `update_columns`
/// so existing `data` JSONB (reason / granted-by / etc.) survives
/// a re-grant.
#[cfg(feature = "postgres")]
pub async fn set_user_perm(
    user_id: i64,
    codename: &str,
    granted: bool,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    set_user_perm_pool(
        user_id,
        codename,
        granted,
        &crate::sql::Pool::from(pool.clone()),
    )
    .await
}

/// v0.38 — tri-dialect counterpart of [`set_user_perm`].
pub async fn set_user_perm_pool(
    user_id: i64,
    codename: &str,
    granted: bool,
    pool: &crate::sql::Pool,
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
    crate::sql::insert_pool(pool, &query).await?;
    Ok(())
}

/// Remove a per-user override, restoring role-based resolution.
#[cfg(feature = "postgres")]
pub async fn clear_user_perm(
    user_id: i64,
    codename: &str,
    pool: &PgPool,
) -> Result<(), TenancyError> {
    crate::sql::__macro_internals::delete_on(
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

/// v0.38 — tri-dialect counterpart of [`clear_user_perm`].
pub async fn clear_user_perm_pool(
    user_id: i64,
    codename: &str,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    crate::sql::delete_pool(
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

/// v0.37 — tri-dialect counterpart of [`user_roles_qs`]. SQL is
/// rendered via the dialect's emitter so identifier quoting +
/// placeholder syntax come out right per backend.
///
/// # Errors
/// Driver / SQL failures from the JOIN-SELECT.
pub async fn user_roles_qs_pool(
    user_id: i64,
    pool: &crate::sql::Pool,
) -> Result<Vec<Role>, sqlx::Error> {
    use sqlx::Row as _;
    let dialect = pool.dialect();
    let roles_t = dialect.quote_ident("rustango_roles");
    let user_roles_t = dialect.quote_ident("rustango_user_roles");
    let p1 = dialect.placeholder(1);
    // No table-aliasing — dialect emitters quote every identifier,
    // and the columns we need are unambiguous since `rustango_roles`
    // is the only side that has them.
    let sql = format!(
        "SELECT r.id, r.name, r.description, r.data \
         FROM {roles_t} r \
         JOIN {user_roles_t} ur ON ur.role_id = r.id \
         WHERE ur.user_id = {p1} \
         ORDER BY r.name"
    );
    let make_role = |id: i64, name: String, description: String, data: serde_json::Value| -> Role {
        Role {
            id: Auto::Set(id),
            name,
            description,
            data,
        }
    };
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let rows = sqlx::query(&sql).bind(user_id).fetch_all(pg).await?;
            rows.iter()
                .map(|row| {
                    Ok(make_role(
                        row.try_get::<i64, _>("id")?,
                        row.try_get("name")?,
                        row.try_get("description")?,
                        row.try_get::<serde_json::Value, _>("data")
                            .unwrap_or_else(|_| serde_json::json!({})),
                    ))
                })
                .collect()
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let rows = sqlx::query(&sql).bind(user_id).fetch_all(my).await?;
            rows.iter()
                .map(|row| {
                    let data: serde_json::Value = row
                        .try_get::<sqlx::types::Json<serde_json::Value>, _>("data")
                        .map(|j| j.0)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    Ok(make_role(
                        row.try_get::<i64, _>("id")?,
                        row.try_get("name")?,
                        row.try_get("description")?,
                        data,
                    ))
                })
                .collect()
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let rows = sqlx::query(&sql).bind(user_id).fetch_all(sq).await?;
            rows.iter()
                .map(|row| {
                    let data: serde_json::Value = row
                        .try_get::<Option<String>, _>("data")
                        .ok()
                        .flatten()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_else(|| serde_json::json!({}));
                    Ok(make_role(
                        row.try_get::<i64, _>("id")?,
                        row.try_get("name")?,
                        row.try_get("description")?,
                        data,
                    ))
                })
                .collect()
        }
    }
}

/// v0.37 — tri-dialect counterpart of [`user_permissions`]. The CTE
/// + UNION ALL shape is supported on PG, MySQL 8+ and SQLite 3.8+,
/// so the query template is the same across backends — only quoting
/// (`"…"` vs `` `…` ``) and placeholder syntax (`$1` vs `?`) differ,
/// and the dialect emitter handles both.
///
/// # Errors
/// Driver / SQL failures from the CTE SELECT.
pub async fn user_permissions_pool(
    uid: i64,
    pool: &crate::sql::Pool,
) -> Result<Vec<String>, TenancyError> {
    use crate::core::SqlValue;
    let dialect = pool.dialect();
    let user_perms_t = dialect.quote_ident("rustango_user_permissions");
    let user_roles_t = dialect.quote_ident("rustango_user_roles");
    let role_perms_t = dialect.quote_ident("rustango_role_permissions");
    let p1 = dialect.placeholder(1);
    let p2 = dialect.placeholder(2);
    let p3 = dialect.placeholder(3);
    let true_lit = dialect.bool_literal(true);
    let false_lit = dialect.bool_literal(false);
    let sql = format!(
        "WITH denied AS ( \
            SELECT codename FROM {user_perms_t} \
            WHERE user_id = {p1} AND granted = {false_lit} \
         ) \
         SELECT DISTINCT codename FROM ( \
            SELECT rp.codename \
            FROM {user_roles_t} ur \
            JOIN {role_perms_t} rp ON rp.role_id = ur.role_id \
            WHERE ur.user_id = {p2} \
              AND rp.codename NOT IN (SELECT codename FROM denied) \
            UNION ALL \
            SELECT codename FROM {user_perms_t} \
            WHERE user_id = {p3} AND granted = {true_lit} \
              AND codename NOT IN (SELECT codename FROM denied) \
         ) effective \
         ORDER BY codename"
    );
    // #561 — was a 3-arm `match pool` doing identical `bind(uid) ×3
    // + fetch_all + try_get::<String, _>("codename")` per backend.
    // Route through the shared `raw_query_pool::<(String,)>` —
    // positional decode of the single-column SELECT.
    let rows = crate::sql::raw_query_pool::<(String,)>(
        &sql,
        vec![SqlValue::I64(uid), SqlValue::I64(uid), SqlValue::I64(uid)],
        pool,
    )
    .await
    .map_err(|e| match e {
        crate::sql::ExecError::Driver(err) => err,
        other => sqlx::Error::Protocol(format!("{other}")),
    })?;
    Ok(rows.into_iter().map(|(codename,)| codename).collect())
}

/// List all roles a user belongs to.
#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
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
/// [`ensure_tables_pool`] so the catalog reflects the current model set.
///
/// # Errors
/// Driver / SQL failures.
#[cfg(feature = "postgres")]
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
        let allowed_actions = entry.schema.default_permissions;
        for (action, verb) in &action_names {
            if !allowed_actions.is_empty() && !allowed_actions.contains(action) {
                continue;
            }
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

/// v0.38 — tri-dialect counterpart of [`auto_create_permissions`].
///
/// Unlike [`ensure_tables_pool`], this is *not* redundant with the
/// bootstrap migration: the migration creates the `rustango_permissions`
/// table but cannot know which models the binary has compiled in.
/// This walks the runtime `inventory::<ModelEntry>` and seeds one row
/// per `(model_table, action)` for every model carrying
/// `#[rustango(permissions)]`. Idempotent via `ON CONFLICT DO NOTHING`
/// (supported on PG, SQLite ≥ 3.24, MySQL via the IGNORE-style
/// `INSERT ... ON CONFLICT(...) DO NOTHING` which sqlx routes through
/// the `insert_pool` IR emitter).
///
/// PG path used a single `UNNEST($1::text[], $2::text[], $3::text[])`
/// to insert every row in one round-trip. SQLite + MySQL have no
/// `UNNEST` of parallel arrays, so the `_pool` variant builds one
/// `InsertQuery` per (table, action) and dispatches via `insert_pool`
/// — N round-trips for N codenames, where N ≤ `(model_count × 4)`.
/// Typical projects have <50 models, so this is <200 short
/// INSERTs at boot, well under a second.
///
/// # Errors
/// Driver / SQL failures from any of the per-row inserts.
pub async fn auto_create_permissions_pool(pool: &crate::sql::Pool) -> Result<(), TenancyError> {
    use crate::core::{inventory, ModelEntry};

    // Seed the framework-reserved codenames first — they aren't
    // model-bound so the inventory loop below doesn't cover them.
    // Issue #311.
    seed_reserved_codename_pool(
        pool,
        AUTH_NAMESPACE,
        ACCESS_ADMIN_CODENAME,
        "Can access framework admin",
    )
    .await?;

    let action_names = [
        ("add", "Can add"),
        ("change", "Can change"),
        ("delete", "Can delete"),
        ("view", "Can view"),
    ];

    for entry in inventory::iter::<ModelEntry> {
        if !entry.schema.permissions {
            continue;
        }
        let table = entry.schema.table;
        let model_name = entry.schema.name;
        // Django Meta.default_permissions — operator-declared subset of
        // the CRUD codename set. Empty slice (the default) means "all
        // four" (matches Django's behavior when the attribute is
        // omitted). When non-empty, the seeder only emits codenames
        // whose action appears in the list. Issue #319 follow-up.
        let allowed_actions = entry.schema.default_permissions;
        let action_allowed = |action: &str| -> bool {
            allowed_actions.is_empty() || allowed_actions.contains(&action)
        };
        // Django Meta.permissions — extra (codename, name) pairs seeded
        // alongside the auto CRUD codenames so apps can declare custom
        // authorization buckets (`("approve", "Can approve posts")`).
        // The codename is stored as `<table>.<codename>` for consistency
        // with the CRUD codenames the loop below seeds.
        for (codename, display) in entry.schema.extra_permissions {
            let dialect = pool.dialect();
            let perm_t = dialect.quote_ident("rustango_permissions");
            let table_col = dialect.quote_ident("table_name");
            let codename_col = dialect.quote_ident("codename");
            let name_col = dialect.quote_ident("name");
            let p1 = dialect.placeholder(1);
            let p2 = dialect.placeholder(2);
            let p3 = dialect.placeholder(3);
            let conflict_tail = dialect.insert_on_conflict_skip(&[&table_col, &codename_col]);
            let sql = format!(
                "INSERT INTO {perm_t} ({table_col}, {codename_col}, {name_col}) \
                 VALUES ({p1}, {p2}, {p3}) \
                 {conflict_tail}"
            );
            let full_codename = format!("{table}.{codename}");
            crate::sql::raw_execute_pool(
                pool,
                &sql,
                vec![
                    SqlValue::from(table.to_owned()),
                    SqlValue::from(full_codename),
                    SqlValue::from((*display).to_owned()),
                ],
            )
            .await
            .map_err(|e| {
                TenancyError::Validation(format!(
                    "auto_create_permissions_pool: INSERT for `{table}.{codename}` failed: {e}"
                ))
            })?;
        }
        for (action, verb) in &action_names {
            if !action_allowed(action) {
                continue;
            }
            // Hand-rolled `INSERT … ON CONFLICT (...) DO NOTHING`
            // via the dialect emitter — `rustango_permissions` isn't
            // a `#[derive(Model)]` so we can't go through the ORM
            // IR (`InsertQuery` needs a `&'static ModelSchema`).
            // Pattern mirrors `audit::audit_select_sql` from v0.37.
            let dialect = pool.dialect();
            let perm_t = dialect.quote_ident("rustango_permissions");
            let table_col = dialect.quote_ident("table_name");
            let codename_col = dialect.quote_ident("codename");
            let name_col = dialect.quote_ident("name");
            let p1 = dialect.placeholder(1);
            let p2 = dialect.placeholder(2);
            let p3 = dialect.placeholder(3);
            // PG / SQLite emit `ON CONFLICT (col1, col2) DO NOTHING`;
            // MySQL emits `ON DUPLICATE KEY UPDATE <pivot> = <pivot>`.
            // The dialect picks the right tail; the unique index on
            // (`table_name`, `codename`) is what both forms target.
            let conflict_tail = dialect.insert_on_conflict_skip(&[&table_col, &codename_col]);
            let sql = format!(
                "INSERT INTO {perm_t} ({table_col}, {codename_col}, {name_col}) \
                 VALUES ({p1}, {p2}, {p3}) \
                 {conflict_tail}"
            );
            let codename = format!("{table}.{action}");
            let display = format!("{verb} {model_name}");
            crate::sql::raw_execute_pool(
                pool,
                &sql,
                vec![
                    SqlValue::from(table.to_owned()),
                    SqlValue::from(codename),
                    SqlValue::from(display),
                ],
            )
            .await
            .map_err(|e| {
                TenancyError::Validation(format!(
                    "auto_create_permissions_pool: INSERT for `{table}.{action}` failed: {e}"
                ))
            })?;
        }
    }
    Ok(())
}

/// Idempotently insert one row into `rustango_permissions` for a
/// non-model-bound (cross-cutting) codename. Used by
/// [`auto_create_permissions_pool`] to seed reserved codenames like
/// [`ACCESS_ADMIN_CODENAME`]. Downstream consuming crates can call
/// this directly at their own migration time to register their own
/// reserved codenames (e.g. `auth.access_cms_admin`).
///
/// # Errors
/// Driver / SQL failures from the INSERT.
pub async fn seed_reserved_codename_pool(
    pool: &crate::sql::Pool,
    namespace: &str,
    codename: &str,
    display_name: &str,
) -> Result<(), TenancyError> {
    let dialect = pool.dialect();
    let perm_t = dialect.quote_ident("rustango_permissions");
    let table_col = dialect.quote_ident("table_name");
    let codename_col = dialect.quote_ident("codename");
    let name_col = dialect.quote_ident("name");
    let p1 = dialect.placeholder(1);
    let p2 = dialect.placeholder(2);
    let p3 = dialect.placeholder(3);
    let conflict_tail = dialect.insert_on_conflict_skip(&[&table_col, &codename_col]);
    let sql = format!(
        "INSERT INTO {perm_t} ({table_col}, {codename_col}, {name_col}) \
         VALUES ({p1}, {p2}, {p3}) \
         {conflict_tail}"
    );
    crate::sql::raw_execute_pool(
        pool,
        &sql,
        vec![
            SqlValue::from(namespace.to_owned()),
            SqlValue::from(codename.to_owned()),
            SqlValue::from(display_name.to_owned()),
        ],
    )
    .await
    .map_err(|e| {
        TenancyError::Validation(format!(
            "seed_reserved_codename_pool: INSERT for `{codename}` failed: {e}"
        ))
    })?;
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
