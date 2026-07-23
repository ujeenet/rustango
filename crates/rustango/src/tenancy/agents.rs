//! Tenant-scoped **MCP agent** identity (epic #1013, Slice 2 / #1015).
//!
//! An `Agent` is a first-class machine identity that lives in a tenant's
//! database (modeled like [`crate::tenancy::User`], not owned by one).
//! It authenticates with a `prefix.secret` credential — generated +
//! verified by [`crate::api_keys`] — and exchanges it for a short-lived
//! scoped JWT (see [`crate::mcp::auth`]).
//!
//! The table is created on demand by [`ensure_agents_table_pool`], exactly
//! like `rustango_api_keys` (`tenancy::auth_backends::ensure_api_keys_table_pool`)
//! — the bootstrap `forward` ops only create `rustango_users`; everything
//! else is ensured per-dialect. `Agent::SCHEMA` is still listed in the
//! tenant bootstrap snapshot so a downstream `make_migrations` sees a
//! complete world.

use crate::sql::{Auto, ExecError, Pool};

/// A tenant-scoped MCP agent. Authenticates with a `prefix.secret`
/// credential; the secret half is argon2id-hashed in `secret_hash` and
/// never stored or returned in the clear.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_agents",
    display = "name",
    admin(
        list_display = "name, active, secret_prefix, created_at",
        search_fields = "name",
        ordering = "name",
        readonly_fields = "secret_prefix, secret_hash, created_at, secret_rotated_at",
    )
)]
pub struct Agent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Login handle. Unique within this tenant.
    #[rustango(max_length = 150, unique)]
    pub name: String,
    /// 8-char public prefix — the lookup half of the credential, stored
    /// in plaintext by design (mirrors `ApiKey.key_prefix`).
    #[rustango(max_length = 16, unique)]
    pub secret_prefix: String,
    /// argon2id hash of the 32-char secret half. Never returned.
    #[rustango(max_length = 255)]
    pub secret_hash: String,
    /// Soft-disable — `false` rejects auth without dropping the row.
    pub active: bool,
    /// Set on INSERT via the per-dialect `DEFAULT NOW()`.
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    /// Timestamp of the last secret rotation. `None` until first rotated.
    pub secret_rotated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Optional owning `rustango_users.id`. `None` for a standalone machine
    /// agent; `Some(uid)` for a **user-owned key** — a personal credential a
    /// member generates so an LLM can act on their behalf. The owner rides
    /// into the agent's scoped JWT (`uid` claim) so tool handlers can scope
    /// work to the member (`ctx.agent.user_id`). Logically references
    /// `rustango_users`; the ensure-DDL owns the schema (like the rest of
    /// this table) so no hard FK is declared.
    pub user_id: Option<i64>,
    /// Flexible per-agent metadata. Never read by the framework.
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
}

// --------------------------------------------------------------- ensure DDL
// Per-dialect `CREATE TABLE IF NOT EXISTS`, mirroring
// `tenancy::auth_backends`'s api-keys constants. No FK — an agent is its
// own identity (not owned by a `rustango_users` row).

const AGENT_ENSURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_agents" (
    "id"                BIGSERIAL    PRIMARY KEY,
    "name"              VARCHAR(150) NOT NULL,
    "secret_prefix"     VARCHAR(16)  NOT NULL,
    "secret_hash"       VARCHAR(255) NOT NULL,
    "active"            BOOLEAN      NOT NULL DEFAULT TRUE,
    "created_at"        TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    "secret_rotated_at" TIMESTAMPTZ,
    "user_id"           BIGINT,
    "data"              JSONB        NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_agents_name_uq"   UNIQUE ("name"),
    CONSTRAINT "rustango_agents_prefix_uq" UNIQUE ("secret_prefix")
);"#;

const AGENT_ENSURE_SQL_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_agents" (
    "id"                INTEGER PRIMARY KEY AUTOINCREMENT,
    "name"              TEXT    NOT NULL,
    "secret_prefix"     TEXT    NOT NULL,
    "secret_hash"       TEXT    NOT NULL,
    "active"            INTEGER NOT NULL DEFAULT 1,
    "created_at"        TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "secret_rotated_at" TEXT,
    "user_id"           INTEGER,
    "data"              TEXT    NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_agents_name_uq"   UNIQUE ("name"),
    CONSTRAINT "rustango_agents_prefix_uq" UNIQUE ("secret_prefix")
);"#;

const AGENT_ENSURE_SQL_MYSQL: &str = r#"
CREATE TABLE IF NOT EXISTS `rustango_agents` (
    `id`                BIGINT AUTO_INCREMENT PRIMARY KEY,
    `name`              VARCHAR(150) NOT NULL,
    `secret_prefix`     VARCHAR(16)  NOT NULL,
    `secret_hash`       VARCHAR(255) NOT NULL,
    `active`            TINYINT(1)   NOT NULL DEFAULT 1,
    `created_at`        DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    `secret_rotated_at` DATETIME(6),
    `user_id`           BIGINT,
    `data`              JSON,
    CONSTRAINT `rustango_agents_name_uq`   UNIQUE (`name`),
    CONSTRAINT `rustango_agents_prefix_uq` UNIQUE (`secret_prefix`)
);"#;

/// Create the `rustango_agents` table if absent (idempotent, per-dialect).
/// Mirrors `tenancy::auth_backends::ensure_api_keys_table_pool`.
///
/// # Errors
/// Propagates the driver error if the `CREATE TABLE` fails.
pub async fn ensure_agents_table_pool(pool: &Pool) -> Result<(), sqlx::Error> {
    let ddl = match pool.dialect().name() {
        "sqlite" => AGENT_ENSURE_SQL_SQLITE,
        "mysql" => AGENT_ENSURE_SQL_MYSQL,
        _ => AGENT_ENSURE_SQL,
    };
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        crate::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .map_err(|e| match e {
                ExecError::Driver(err) => err,
                other => sqlx::Error::Protocol(format!("{other}")),
            })?;
    }
    // Additive: `CREATE TABLE IF NOT EXISTS` won't add `user_id` to a table
    // created before this column existed. Probe for it and `ADD COLUMN` once
    // if missing, so upgrading in place is transparent (no migration step).
    ensure_user_id_column(pool).await?;
    Ok(())
}

/// Add `rustango_agents.user_id` if an older table predates it. Probe with a
/// cheap zero-row select (dialect-agnostic); only `ALTER` when the column is
/// absent. The added column is nullable with no default, so it's a metadata-
/// only change on every backend.
async fn ensure_user_id_column(pool: &Pool) -> Result<(), sqlx::Error> {
    // Identifier quoting differs by dialect (MySQL uses backticks).
    let (tbl, col, col_type) = match pool.dialect().name() {
        "mysql" => ("`rustango_agents`", "`user_id`", "BIGINT"),
        "sqlite" => (r#""rustango_agents""#, r#""user_id""#, "INTEGER"),
        _ => (r#""rustango_agents""#, r#""user_id""#, "BIGINT"),
    };
    let probe = format!("SELECT {col} FROM {tbl} LIMIT 0");
    if crate::sql::raw_execute_pool(pool, &probe, Vec::new())
        .await
        .is_ok()
    {
        return Ok(()); // column already present
    }
    let alter = format!("ALTER TABLE {tbl} ADD COLUMN {col} {col_type}");
    crate::sql::raw_execute_pool(pool, &alter, Vec::new())
        .await
        .map_err(|e| match e {
            ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })?;
    Ok(())
}

// ------------------------------------------------------------- operations

/// Outcome of [`create_agent_pool`] / [`rotate_agent_secret_pool`] — the
/// stored row plus the **one-time** full `prefix.secret` token to show the
/// operator once (never recoverable afterward).
pub struct AgentSecret {
    pub agent: Agent,
    /// `prefix.secret` — present this to the operator a single time.
    pub token: String,
}

/// Errors from the agent operations.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent `{0}` already exists in this tenant")]
    Duplicate(String),
    #[error("agent `{0}` not found in this tenant")]
    NotFound(String),
    #[error("secret generation failed: {0}")]
    Secret(String),
    #[error(transparent)]
    Db(#[from] ExecError),
    #[error(transparent)]
    Driver(#[from] sqlx::Error),
    #[error(transparent)]
    Tenancy(#[from] super::error::TenancyError),
}

/// Generate a fresh `prefix.secret` credential: returns
/// `(full_token, prefix, secret_hash)`. Mirrors
/// `tenancy::auth_backends::create_api_key`'s OS-CSPRNG + argon2id path —
/// 4-byte hex prefix (lookup half) + 16-byte hex secret (bearer half),
/// hashed with [`crate::tenancy::password::hash`]. No `api_keys` feature
/// needed (the framework's own argon2id is always on with `tenancy`).
fn generate_credential() -> Result<(String, String, String), AgentError> {
    use rand::rngs::OsRng;
    use rand::RngCore;

    let mut prefix_bytes = [0u8; 4];
    OsRng.fill_bytes(&mut prefix_bytes);
    let prefix = to_hex(&prefix_bytes);
    let mut secret_bytes = [0u8; 16];
    OsRng.fill_bytes(&mut secret_bytes);
    let secret = to_hex(&secret_bytes);
    let hash = super::password::hash(&secret).map_err(|e| AgentError::Secret(e.to_string()))?;
    Ok((format!("{prefix}.{secret}"), prefix, hash))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Create a new agent with a freshly generated credential. The returned
/// [`AgentSecret::token`] is the only time the secret is available.
///
/// # Errors
/// [`AgentError::Duplicate`] if the name is taken; otherwise a DB / secret
/// generation error.
pub async fn create_agent_pool(pool: &Pool, name: &str) -> Result<AgentSecret, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_agents_table_pool(pool).await?;

    let existing: Vec<Agent> = Agent::objects()
        .where_(Agent::name.eq(name))
        .limit(1)
        .fetch(pool)
        .await?;
    if !existing.is_empty() {
        return Err(AgentError::Duplicate(name.to_owned()));
    }

    let (token, prefix, hash) = generate_credential()?;

    let mut agent = Agent {
        id: Auto::default(),
        name: name.to_owned(),
        secret_prefix: prefix,
        secret_hash: hash,
        active: true,
        created_at: Auto::default(),
        secret_rotated_at: None,
        user_id: None,
        data: serde_json::json!({}),
    };
    agent.insert_pool(pool).await?;
    Ok(AgentSecret { agent, token })
}

/// Rotate an existing agent's secret — issues a new credential, invalidating
/// the old one. Returns the new one-time token.
///
/// # Errors
/// [`AgentError::NotFound`] if no agent with that name exists.
pub async fn rotate_agent_secret_pool(pool: &Pool, name: &str) -> Result<AgentSecret, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_agents_table_pool(pool).await?;

    let mut agent: Agent = Agent::objects()
        .where_(Agent::name.eq(name))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AgentError::NotFound(name.to_owned()))?;

    let (token, prefix, hash) = generate_credential()?;
    agent.secret_prefix = prefix;
    agent.secret_hash = hash;
    agent.secret_rotated_at = Some(chrono::Utc::now());
    agent.save_pool(pool).await?;
    Ok(AgentSecret { agent, token })
}

/// List all agents in the tenant, ordered by name.
///
/// # Errors
/// Propagates DB errors.
pub async fn list_agents_pool(pool: &Pool) -> Result<Vec<Agent>, AgentError> {
    use crate::sql::FetcherPool as _;
    ensure_agents_table_pool(pool).await?;
    let agents = Agent::objects()
        .order_by(&[("name", false)])
        .fetch(pool)
        .await?;
    Ok(agents)
}

/// Verify a `{name, secret}` credential against the tenant's agents.
/// `secret` may be either the full `prefix.secret` token or the bare
/// secret half. Returns the active matching agent, or `None` on any
/// mismatch (unknown name, inactive, or bad secret) — fail-closed.
///
/// # Errors
/// Propagates DB errors only; an unverifiable secret is `Ok(None)`, not an
/// error, so callers can return a uniform `401`.
pub async fn authenticate_agent_pool(
    pool: &Pool,
    name: &str,
    secret: &str,
) -> Result<Option<Agent>, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_agents_table_pool(pool).await?;

    // Accept either `prefix.secret` or the bare secret half (secrets are
    // hex, so the part after the last `.` is the secret).
    let secret_half = secret.rsplit('.').next().unwrap_or(secret);

    let Some(agent) = Agent::objects()
        .where_(Agent::name.eq(name))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .filter(|a| a.active)
    else {
        // Unknown / inactive agent: still spend an argon2 verification against a
        // fixed dummy hash so the response time doesn't reveal whether the agent
        // name exists (timing oracle → agent enumeration). #1099.
        super::password::verify_dummy(secret_half);
        return Ok(None);
    };

    match super::password::verify(secret_half, &agent.secret_hash) {
        Ok(true) => Ok(Some(agent)),
        _ => Ok(None),
    }
}

// ============================================================= skills (Slice 4)
// Skills are the grant unit (epic #1013, Slice 4 / #1017): a skill bundles a
// set of tools (+ a prompt + resources, Slice 5). Granting a skill to an agent
// flattens its tools into the agent's JWT `tools` claim at token-issue time,
// which is what the `tools/list` / `tools/call` authorization reads.

/// A named bundle of tools (+ an instruction prompt, Slice 5). The grant unit.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_agent_skills",
    display = "codename",
    admin(
        list_display = "codename, name, description",
        search_fields = "codename, name",
        ordering = "codename",
    )
)]
pub struct AgentSkill {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Stable identifier referenced by grants. Unique within the tenant.
    #[rustango(max_length = 100, unique)]
    pub codename: String,
    #[rustango(max_length = 150)]
    pub name: String,
    #[rustango(max_length = 500)]
    pub description: String,
    /// MCP prompt body surfaced via `prompts/get` (Slice 5).
    pub instructions: String,
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
}

/// One tool name belonging to a skill.
#[derive(crate::Model, Debug, Clone)]
#[rustango(table = "rustango_agent_skill_tools")]
pub struct AgentSkillTool {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "rustango_agent_skills", on = "id", on_delete = "cascade")]
    pub skill_id: i64,
    #[rustango(max_length = 150)]
    pub tool_name: String,
}

/// A skill granted to an agent. `UNIQUE(agent_id, skill_id)`.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_agent_grants",
    admin(list_display = "agent_id, skill_id", ordering = "agent_id")
)]
pub struct AgentGrant {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "rustango_agents", on = "id", on_delete = "cascade")]
    pub agent_id: i64,
    #[rustango(fk = "rustango_agent_skills", on = "id", on_delete = "cascade")]
    pub skill_id: i64,
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
}

/// Maps a skill to a user **permission codename**. A user-owned key is
/// granted the skill (its tools + prompt + resources) when the owning user
/// holds any mapped permission — so MCP capabilities follow the tenant's
/// existing RBAC. Standalone (machine) agents ignore this; they use explicit
/// [`AgentGrant`]s only.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_agent_skill_permissions",
    admin(list_display = "skill_id, permission_codename", ordering = "skill_id")
)]
pub struct AgentSkillPermission {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "rustango_agent_skills", on = "id", on_delete = "cascade")]
    pub skill_id: i64,
    /// A `rustango_permissions` codename (e.g. `ib_member_profile.change`).
    #[rustango(max_length = 150)]
    pub permission_codename: String,
}

const SKILLS_ENSURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_agent_skills" (
    "id"           BIGSERIAL    PRIMARY KEY,
    "codename"     VARCHAR(100) NOT NULL,
    "name"         VARCHAR(150) NOT NULL DEFAULT '',
    "description"  VARCHAR(500) NOT NULL DEFAULT '',
    "instructions" TEXT         NOT NULL DEFAULT '',
    "data"         JSONB        NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_agent_skills_codename_uq" UNIQUE ("codename")
);
CREATE TABLE IF NOT EXISTS "rustango_agent_skill_tools" (
    "id"        BIGSERIAL    PRIMARY KEY,
    "skill_id"  BIGINT       NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "tool_name" VARCHAR(150) NOT NULL
);
CREATE TABLE IF NOT EXISTS "rustango_agent_grants" (
    "id"       BIGSERIAL PRIMARY KEY,
    "agent_id" BIGINT    NOT NULL REFERENCES "rustango_agents"("id")       ON DELETE CASCADE,
    "skill_id" BIGINT    NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "data"     JSONB     NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_agent_grants_uq" UNIQUE ("agent_id", "skill_id")
);
CREATE TABLE IF NOT EXISTS "rustango_agent_skill_permissions" (
    "id"                  BIGSERIAL    PRIMARY KEY,
    "skill_id"            BIGINT       NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "permission_codename" VARCHAR(150) NOT NULL,
    CONSTRAINT "rustango_agent_skill_permissions_uq" UNIQUE ("skill_id", "permission_codename")
);"#;

const SKILLS_ENSURE_SQL_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_agent_skills" (
    "id"           INTEGER PRIMARY KEY AUTOINCREMENT,
    "codename"     TEXT NOT NULL,
    "name"         TEXT NOT NULL DEFAULT '',
    "description"  TEXT NOT NULL DEFAULT '',
    "instructions" TEXT NOT NULL DEFAULT '',
    "data"         TEXT NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_agent_skills_codename_uq" UNIQUE ("codename")
);
CREATE TABLE IF NOT EXISTS "rustango_agent_skill_tools" (
    "id"        INTEGER PRIMARY KEY AUTOINCREMENT,
    "skill_id"  INTEGER NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "tool_name" TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS "rustango_agent_grants" (
    "id"       INTEGER PRIMARY KEY AUTOINCREMENT,
    "agent_id" INTEGER NOT NULL REFERENCES "rustango_agents"("id")       ON DELETE CASCADE,
    "skill_id" INTEGER NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "data"     TEXT    NOT NULL DEFAULT '{}',
    CONSTRAINT "rustango_agent_grants_uq" UNIQUE ("agent_id", "skill_id")
);
CREATE TABLE IF NOT EXISTS "rustango_agent_skill_permissions" (
    "id"                  INTEGER PRIMARY KEY AUTOINCREMENT,
    "skill_id"            INTEGER NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "permission_codename" TEXT    NOT NULL,
    CONSTRAINT "rustango_agent_skill_permissions_uq" UNIQUE ("skill_id", "permission_codename")
);"#;

const SKILLS_ENSURE_SQL_MYSQL: &str = r#"
CREATE TABLE IF NOT EXISTS `rustango_agent_skills` (
    `id`           BIGINT AUTO_INCREMENT PRIMARY KEY,
    `codename`     VARCHAR(100) NOT NULL,
    `name`         VARCHAR(150) NOT NULL DEFAULT '',
    `description`  VARCHAR(500) NOT NULL DEFAULT '',
    `instructions` TEXT         NOT NULL,
    `data`         JSON,
    CONSTRAINT `rustango_agent_skills_codename_uq` UNIQUE (`codename`)
);
CREATE TABLE IF NOT EXISTS `rustango_agent_skill_tools` (
    `id`        BIGINT AUTO_INCREMENT PRIMARY KEY,
    `skill_id`  BIGINT       NOT NULL,
    `tool_name` VARCHAR(150) NOT NULL,
    CONSTRAINT `rustango_agent_skill_tools_fk` FOREIGN KEY (`skill_id`)
        REFERENCES `rustango_agent_skills`(`id`) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS `rustango_agent_grants` (
    `id`       BIGINT AUTO_INCREMENT PRIMARY KEY,
    `agent_id` BIGINT NOT NULL,
    `skill_id` BIGINT NOT NULL,
    `data`     JSON,
    CONSTRAINT `rustango_agent_grants_uq` UNIQUE (`agent_id`, `skill_id`),
    CONSTRAINT `rustango_agent_grants_fk_agent` FOREIGN KEY (`agent_id`)
        REFERENCES `rustango_agents`(`id`) ON DELETE CASCADE,
    CONSTRAINT `rustango_agent_grants_fk_skill` FOREIGN KEY (`skill_id`)
        REFERENCES `rustango_agent_skills`(`id`) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS `rustango_agent_skill_permissions` (
    `id`                  BIGINT AUTO_INCREMENT PRIMARY KEY,
    `skill_id`            BIGINT       NOT NULL,
    `permission_codename` VARCHAR(150) NOT NULL,
    CONSTRAINT `rustango_agent_skill_permissions_uq` UNIQUE (`skill_id`, `permission_codename`),
    CONSTRAINT `rustango_agent_skill_permissions_fk` FOREIGN KEY (`skill_id`)
        REFERENCES `rustango_agent_skills`(`id`) ON DELETE CASCADE
);"#;

/// Create the skill / skill-tool / grant tables if absent (idempotent,
/// per-dialect). Mirrors [`ensure_agents_table_pool`].
///
/// # Errors
/// Propagates the driver error if a `CREATE TABLE` fails.
pub async fn ensure_skill_tables_pool(pool: &Pool) -> Result<(), sqlx::Error> {
    let ddl = match pool.dialect().name() {
        "sqlite" => SKILLS_ENSURE_SQL_SQLITE,
        "mysql" => SKILLS_ENSURE_SQL_MYSQL,
        _ => SKILLS_ENSURE_SQL,
    };
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        crate::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .map_err(|e| match e {
                ExecError::Driver(err) => err,
                other => sqlx::Error::Protocol(format!("{other}")),
            })?;
    }
    Ok(())
}

/// Create a skill bundling `tools` (by name). `name`/`description`/
/// `instructions` are optional metadata.
///
/// # Errors
/// [`AgentError::Duplicate`] if the codename is taken.
pub async fn create_skill_pool(
    pool: &Pool,
    codename: &str,
    name: &str,
    description: &str,
    instructions: &str,
    tools: &[String],
) -> Result<AgentSkill, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_skill_tables_pool(pool).await?;
    let existing: Vec<AgentSkill> = AgentSkill::objects()
        .where_(AgentSkill::codename.eq(codename))
        .limit(1)
        .fetch(pool)
        .await?;
    if !existing.is_empty() {
        return Err(AgentError::Duplicate(codename.to_owned()));
    }

    let mut skill = AgentSkill {
        id: Auto::default(),
        codename: codename.to_owned(),
        name: name.to_owned(),
        description: description.to_owned(),
        instructions: instructions.to_owned(),
        data: serde_json::json!({}),
    };
    skill.insert_pool(pool).await?;
    let skill_id = skill.id.get().copied().unwrap_or_default();

    for tool in tools {
        let mut row = AgentSkillTool {
            id: Auto::default(),
            skill_id,
            tool_name: tool.clone(),
        };
        row.insert_pool(pool).await?;
    }
    Ok(skill)
}

/// List all skills in the tenant, ordered by codename.
///
/// # Errors
/// Propagates DB errors.
pub async fn list_skills_pool(pool: &Pool) -> Result<Vec<AgentSkill>, AgentError> {
    use crate::sql::FetcherPool as _;
    ensure_skill_tables_pool(pool).await?;
    Ok(AgentSkill::objects()
        .order_by(&[("codename", false)])
        .fetch(pool)
        .await?)
}

/// Grant `skill_codename` to the agent named `agent_name`. Idempotent — a
/// duplicate grant is a no-op.
///
/// # Errors
/// [`AgentError::NotFound`] if the agent or skill doesn't exist.
pub async fn grant_skill_pool(
    pool: &Pool,
    slug: &str,
    agent_name: &str,
    skill_codename: &str,
) -> Result<(), AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_agents_table_pool(pool).await?;
    ensure_skill_tables_pool(pool).await?;

    let (agent_id, skill_id) = resolve_ids(pool, agent_name, skill_codename).await?;

    let existing: Vec<AgentGrant> = AgentGrant::objects()
        .where_(AgentGrant::agent_id.eq(agent_id))
        .where_(AgentGrant::skill_id.eq(skill_id))
        .limit(1)
        .fetch(pool)
        .await?;
    if existing.is_empty() {
        let mut grant = AgentGrant {
            id: Auto::default(),
            agent_id,
            skill_id,
            data: serde_json::json!({}),
        };
        grant.insert_pool(pool).await?;
        notify_grants_changed(slug, agent_id);
    }
    Ok(())
}

/// Revoke `skill_codename` from `agent_name`. No-op if not granted.
///
/// # Errors
/// [`AgentError::NotFound`] if the agent or skill doesn't exist.
pub async fn revoke_skill_pool(
    pool: &Pool,
    slug: &str,
    agent_name: &str,
    skill_codename: &str,
) -> Result<(), AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_agents_table_pool(pool).await?;
    ensure_skill_tables_pool(pool).await?;

    let (agent_id, skill_id) = resolve_ids(pool, agent_name, skill_codename).await?;
    let grants: Vec<AgentGrant> = AgentGrant::objects()
        .where_(AgentGrant::agent_id.eq(agent_id))
        .where_(AgentGrant::skill_id.eq(skill_id))
        .fetch(pool)
        .await?;
    let had_grant = !grants.is_empty();
    for grant in grants {
        grant.delete_pool(pool).await?;
    }
    if had_grant {
        notify_grants_changed(slug, agent_id);
    }
    Ok(())
}

/// Notify in-process MCP clients that an agent's granted tools / prompts /
/// resources changed (review fix #1093). No-op without the `mcp` feature and
/// a no-op in a separate process (e.g. the `manage` CLI) — see the
/// notifications module's cross-process caveat.
#[allow(unused_variables)]
fn notify_grants_changed(slug: &str, agent_id: i64) {
    #[cfg(feature = "mcp")]
    {
        crate::mcp::notify_tools_list_changed(slug, Some(agent_id));
        crate::mcp::notify_prompts_list_changed(slug, Some(agent_id));
        crate::mcp::notify_resources_list_changed(slug, Some(agent_id));
    }
}

/// Resolve `(agent_id, skill_id)` from human identifiers, erroring if either
/// is missing.
async fn resolve_ids(
    pool: &Pool,
    agent_name: &str,
    skill_codename: &str,
) -> Result<(i64, i64), AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    let agent = Agent::objects()
        .where_(Agent::name.eq(agent_name))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AgentError::NotFound(agent_name.to_owned()))?;
    let skill = AgentSkill::objects()
        .where_(AgentSkill::codename.eq(skill_codename))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AgentError::NotFound(skill_codename.to_owned()))?;
    Ok((
        agent.id.get().copied().unwrap_or_default(),
        skill.id.get().copied().unwrap_or_default(),
    ))
}

/// Resolve an agent's granted skills → `(skill codenames, flattened+deduped
/// tool names)`. Called at token-issue so the JWT carries the agent's
/// effective tool set. An agent with no grants gets empty vecs (fail-closed).
///
/// # Errors
/// Propagates DB errors.
pub async fn resolve_agent_grants_pool(
    pool: &Pool,
    agent_id: i64,
) -> Result<(Vec<String>, Vec<String>), AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_skill_tables_pool(pool).await?;

    let grants: Vec<AgentGrant> = AgentGrant::objects()
        .where_(AgentGrant::agent_id.eq(agent_id))
        .fetch(pool)
        .await?;
    let skill_ids: Vec<i64> = grants.iter().map(|g| g.skill_id).collect();
    if skill_ids.is_empty() {
        return Ok((vec![], vec![]));
    }

    let skills: Vec<AgentSkill> = AgentSkill::objects()
        .where_(AgentSkill::id.is_in(skill_ids.clone()))
        .fetch(pool)
        .await?;
    let skill_codenames: Vec<String> = skills.into_iter().map(|s| s.codename).collect();

    let skill_tools: Vec<AgentSkillTool> = AgentSkillTool::objects()
        .where_(AgentSkillTool::skill_id.is_in(skill_ids))
        .fetch(pool)
        .await?;
    let mut tools: Vec<String> = Vec::new();
    for st in skill_tools {
        if !tools.contains(&st.tool_name) {
            tools.push(st.tool_name);
        }
    }
    Ok((skill_codenames, tools))
}

// =========================================== permission-driven capabilities
// User-owned keys derive their MCP capabilities from the tenant's RBAC: a
// skill is mapped to one or more permission codenames, and the key's owning
// user is granted that skill (its tools + prompt + resources) when they hold
// any mapped permission. Resolved fresh at token-mint so the JWT reflects the
// user's current entitlements.

/// Map `skill_codename` to a `permission_codename`. Idempotent — a duplicate
/// mapping is a no-op. A user-owned key whose owner holds this permission is
/// then granted the skill at token-issue.
///
/// # Errors
/// [`AgentError::NotFound`] if the skill codename doesn't exist.
pub async fn map_skill_to_permission_pool(
    pool: &Pool,
    skill_codename: &str,
    permission_codename: &str,
) -> Result<(), AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_skill_tables_pool(pool).await?;
    let skill_id = AgentSkill::objects()
        .where_(AgentSkill::codename.eq(skill_codename))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AgentError::NotFound(skill_codename.to_owned()))?
        .id
        .get()
        .copied()
        .unwrap_or_default();

    let existing: Vec<AgentSkillPermission> = AgentSkillPermission::objects()
        .where_(AgentSkillPermission::skill_id.eq(skill_id))
        .where_(AgentSkillPermission::permission_codename.eq(permission_codename))
        .limit(1)
        .fetch(pool)
        .await?;
    if existing.is_empty() {
        let mut row = AgentSkillPermission {
            id: Auto::default(),
            skill_id,
            permission_codename: permission_codename.to_owned(),
        };
        row.insert_pool(pool).await?;
    }
    Ok(())
}

/// Remove a skill↔permission mapping. No-op if absent.
///
/// # Errors
/// [`AgentError::NotFound`] if the skill codename doesn't exist.
pub async fn unmap_skill_from_permission_pool(
    pool: &Pool,
    skill_codename: &str,
    permission_codename: &str,
) -> Result<(), AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_skill_tables_pool(pool).await?;
    let Some(skill_id) = AgentSkill::objects()
        .where_(AgentSkill::codename.eq(skill_codename))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .and_then(|s| s.id.get().copied())
    else {
        return Err(AgentError::NotFound(skill_codename.to_owned()));
    };
    let rows: Vec<AgentSkillPermission> = AgentSkillPermission::objects()
        .where_(AgentSkillPermission::skill_id.eq(skill_id))
        .where_(AgentSkillPermission::permission_codename.eq(permission_codename))
        .fetch(pool)
        .await?;
    for row in rows {
        row.delete_pool(pool).await?;
    }
    Ok(())
}

/// Resolve a **user-owned** agent's effective `(skill codenames, tools)`:
/// its explicit [`AgentGrant`]s (if any) unioned with every skill mapped to a
/// permission the owning `user_id` currently holds. This is what
/// [`crate::mcp`] bakes into the scoped JWT so `tools`, `prompts`, and
/// `resources` are all gated by the tenant's RBAC.
///
/// # Errors
/// Propagates DB / permission-lookup errors.
pub async fn resolve_user_agent_grants_pool(
    pool: &Pool,
    agent_id: i64,
    user_id: i64,
) -> Result<(Vec<String>, Vec<String>), AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    // Start from any explicit grants (an app may still pin a skill directly).
    let (mut skills, mut tools) = resolve_agent_grants_pool(pool, agent_id).await?;

    // Skills the owning user is entitled to via their permissions.
    let perms = super::user_permissions_pool(user_id, pool).await?;
    if perms.is_empty() {
        return Ok((skills, tools));
    }

    let mapped: Vec<AgentSkillPermission> = AgentSkillPermission::objects()
        .where_(AgentSkillPermission::permission_codename.is_in(perms))
        .fetch(pool)
        .await?;
    let mut skill_ids: Vec<i64> = Vec::new();
    for m in mapped {
        if !skill_ids.contains(&m.skill_id) {
            skill_ids.push(m.skill_id);
        }
    }
    if skill_ids.is_empty() {
        return Ok((skills, tools));
    }

    let entitled: Vec<AgentSkill> = AgentSkill::objects()
        .where_(AgentSkill::id.is_in(skill_ids.clone()))
        .fetch(pool)
        .await?;
    for s in entitled {
        if !skills.contains(&s.codename) {
            skills.push(s.codename);
        }
    }
    let entitled_tools: Vec<AgentSkillTool> = AgentSkillTool::objects()
        .where_(AgentSkillTool::skill_id.is_in(skill_ids))
        .fetch(pool)
        .await?;
    for st in entitled_tools {
        if !tools.contains(&st.tool_name) {
            tools.push(st.tool_name);
        }
    }
    Ok((skills, tools))
}

// ================================================================ user keys
// A "user key" is a personal, user-owned [`Agent`]: a member generates one so
// an LLM/MCP client can act on their behalf. Its capabilities are entirely
// permission-driven (see [`resolve_user_agent_grants_pool`]); the app never
// pins tools onto it.

/// Allocate a per-tenant-unique agent name for `user_id`'s new key.
async fn unique_key_name(pool: &Pool, user_id: i64) -> Result<String, AgentError> {
    use crate::sql::FetcherPool as _;
    use rand::rngs::OsRng;
    use rand::RngCore;

    for _ in 0..6 {
        let mut b = [0u8; 5];
        OsRng.fill_bytes(&mut b);
        let name = format!("uk_{user_id}_{}", to_hex(&b));
        let taken: Vec<Agent> = Agent::objects()
            .filter("name", name.clone())
            .limit(1)
            .fetch(pool)
            .await?;
        if taken.is_empty() {
            return Ok(name);
        }
    }
    Err(AgentError::Secret(
        "could not allocate a unique key name".to_owned(),
    ))
}

/// Create a personal, user-owned MCP key. The returned [`AgentSecret::token`]
/// (`prefix.secret`) is shown once and never recoverable. Capabilities are
/// resolved from the owner's permissions at token-issue — nothing to pin here.
///
/// # Errors
/// A DB / secret-generation error.
pub async fn create_user_key_pool(
    pool: &Pool,
    user_id: i64,
    label: &str,
) -> Result<AgentSecret, AgentError> {
    ensure_agents_table_pool(pool).await?;
    let name = unique_key_name(pool, user_id).await?;
    let (token, prefix, hash) = generate_credential()?;
    let mut agent = Agent {
        id: Auto::default(),
        name,
        secret_prefix: prefix,
        secret_hash: hash,
        active: true,
        created_at: Auto::default(),
        secret_rotated_at: None,
        user_id: Some(user_id),
        data: serde_json::json!({ "kind": "user_key", "label": label }),
    };
    agent.insert_pool(pool).await?;
    Ok(AgentSecret { agent, token })
}

/// List a user's personal keys, newest first.
///
/// # Errors
/// Propagates DB errors.
pub async fn list_user_keys_pool(pool: &Pool, user_id: i64) -> Result<Vec<Agent>, AgentError> {
    use crate::sql::FetcherPool as _;
    ensure_agents_table_pool(pool).await?;
    Ok(Agent::objects()
        .filter("user_id", user_id)
        .order_by(&[("created_at", true)])
        .fetch(pool)
        .await?)
}

/// Revoke (delete) one of `user_id`'s keys by agent id. Verifies ownership —
/// a key that isn't the user's is reported as [`AgentError::NotFound`], not
/// deleted. Explicit grants are removed first (not relying on FK cascade,
/// which some SQLite connections don't enforce).
///
/// # Errors
/// [`AgentError::NotFound`] if the key doesn't exist or isn't owned by
/// `user_id`.
pub async fn revoke_user_key_pool(
    pool: &Pool,
    user_id: i64,
    agent_id: i64,
) -> Result<(), AgentError> {
    use crate::sql::FetcherPool as _;

    ensure_agents_table_pool(pool).await?;
    let agent = Agent::objects()
        .filter("id", agent_id)
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .filter(|a| a.user_id == Some(user_id))
        .ok_or_else(|| AgentError::NotFound(format!("key #{agent_id}")))?;

    let grants: Vec<AgentGrant> = AgentGrant::objects()
        .filter("agent_id", agent_id)
        .fetch(pool)
        .await?;
    for g in grants {
        g.delete_pool(pool).await?;
    }
    agent.delete_pool(pool).await?;
    Ok(())
}

// ====================================================== resources (Slice 5)

/// A resource attached to a skill (epic #1013, Slice 5 / #1018). The body is
/// carried in `data.text`; `mime` is the advertised MIME type.
#[derive(crate::Model, Debug, Clone)]
#[rustango(table = "rustango_agent_skill_resources")]
pub struct AgentSkillResource {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "rustango_agent_skills", on = "id", on_delete = "cascade")]
    pub skill_id: i64,
    #[rustango(max_length = 500)]
    pub resource_uri: String,
    #[rustango(max_length = 100)]
    pub mime: String,
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
}

const RESOURCES_ENSURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_agent_skill_resources" (
    "id"           BIGSERIAL    PRIMARY KEY,
    "skill_id"     BIGINT       NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "resource_uri" VARCHAR(500) NOT NULL,
    "mime"         VARCHAR(100) NOT NULL DEFAULT 'text/plain',
    "data"         JSONB        NOT NULL DEFAULT '{}'
);"#;

const RESOURCES_ENSURE_SQL_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_agent_skill_resources" (
    "id"           INTEGER PRIMARY KEY AUTOINCREMENT,
    "skill_id"     INTEGER NOT NULL REFERENCES "rustango_agent_skills"("id") ON DELETE CASCADE,
    "resource_uri" TEXT    NOT NULL,
    "mime"         TEXT    NOT NULL DEFAULT 'text/plain',
    "data"         TEXT    NOT NULL DEFAULT '{}'
);"#;

const RESOURCES_ENSURE_SQL_MYSQL: &str = r#"
CREATE TABLE IF NOT EXISTS `rustango_agent_skill_resources` (
    `id`           BIGINT AUTO_INCREMENT PRIMARY KEY,
    `skill_id`     BIGINT       NOT NULL,
    `resource_uri` VARCHAR(500) NOT NULL,
    `mime`         VARCHAR(100) NOT NULL DEFAULT 'text/plain',
    `data`         JSON,
    CONSTRAINT `rustango_agent_skill_resources_fk` FOREIGN KEY (`skill_id`)
        REFERENCES `rustango_agent_skills`(`id`) ON DELETE CASCADE
);"#;

async fn ensure_resources_table_pool(pool: &Pool) -> Result<(), sqlx::Error> {
    let ddl = match pool.dialect().name() {
        "sqlite" => RESOURCES_ENSURE_SQL_SQLITE,
        "mysql" => RESOURCES_ENSURE_SQL_MYSQL,
        _ => RESOURCES_ENSURE_SQL,
    };
    for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        crate::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .map_err(|e| match e {
                ExecError::Driver(err) => err,
                other => sqlx::Error::Protocol(format!("{other}")),
            })?;
    }
    Ok(())
}

/// Attach a resource (uri + mime + text body) to a skill.
///
/// # Errors
/// [`AgentError::NotFound`] if the skill codename doesn't exist.
pub async fn add_skill_resource_pool(
    pool: &Pool,
    skill_codename: &str,
    uri: &str,
    mime: &str,
    body: &str,
) -> Result<AgentSkillResource, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    ensure_skill_tables_pool(pool).await?;
    ensure_resources_table_pool(pool).await?;

    let skill_id = AgentSkill::objects()
        .where_(AgentSkill::codename.eq(skill_codename))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AgentError::NotFound(skill_codename.to_owned()))?
        .id
        .get()
        .copied()
        .unwrap_or_default();

    let mut res = AgentSkillResource {
        id: Auto::default(),
        skill_id,
        resource_uri: uri.to_owned(),
        mime: mime.to_owned(),
        data: serde_json::json!({ "text": body }),
    };
    res.insert_pool(pool).await?;
    Ok(res)
}

/// Fetch the granted skills (by codename) — each is an MCP prompt source
/// (its `instructions` is the prompt body).
///
/// # Errors
/// Propagates DB errors.
pub async fn skills_by_codenames_pool(
    pool: &Pool,
    codenames: &[String],
) -> Result<Vec<AgentSkill>, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    ensure_skill_tables_pool(pool).await?;
    if codenames.is_empty() {
        return Ok(vec![]);
    }
    Ok(AgentSkill::objects()
        .where_(AgentSkill::codename.is_in(codenames.to_vec()))
        .order_by(&[("codename", false)])
        .fetch(pool)
        .await?)
}

/// Fetch the resources of the agent's granted skills (by skill codename).
///
/// # Errors
/// Propagates DB errors.
pub async fn resources_for_skills_pool(
    pool: &Pool,
    codenames: &[String],
) -> Result<Vec<AgentSkillResource>, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    ensure_skill_tables_pool(pool).await?;
    ensure_resources_table_pool(pool).await?;
    if codenames.is_empty() {
        return Ok(vec![]);
    }
    let skill_ids: Vec<i64> = AgentSkill::objects()
        .where_(AgentSkill::codename.is_in(codenames.to_vec()))
        .fetch(pool)
        .await?
        .into_iter()
        .map(|s| s.id.get().copied().unwrap_or_default())
        .collect();
    if skill_ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(AgentSkillResource::objects()
        .where_(AgentSkillResource::skill_id.is_in(skill_ids))
        .fetch(pool)
        .await?)
}
