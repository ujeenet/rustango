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

    let Some(agent) = Agent::objects()
        .where_(Agent::name.eq(name))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .filter(|a| a.active)
    else {
        return Ok(None);
    };

    // Accept either `prefix.secret` or the bare secret half (secrets are
    // hex, so the part after the last `.` is the secret).
    let secret_half = secret.rsplit('.').next().unwrap_or(secret);
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
    }
    Ok(())
}

/// Revoke `skill_codename` from `agent_name`. No-op if not granted.
///
/// # Errors
/// [`AgentError::NotFound`] if the agent or skill doesn't exist.
pub async fn revoke_skill_pool(
    pool: &Pool,
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
    for grant in grants {
        grant.delete_pool(pool).await?;
    }
    Ok(())
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
