//! Tenant-scoped **MCP agent** identity (epic #1013, Slice 2 / #1015).
//!
//! An `Agent` is a first-class machine identity that lives in a tenant's
//! database (modeled like [`crate::tenancy::User`], not owned by one).
//! It authenticates with a `prefix.secret` credential — generated +
//! verified by [`crate::api_keys`] — and exchanges it for a short-lived
//! scoped JWT (see [`crate::mcp::auth`]).
//!
//! Every model here is `#[derive(Model)]`, `managed`, and lives on a
//! `rustango_*` table, so all its tables are emitted as ordinary **system
//! migrations** (and, in tests, materialized by
//! [`crate::testkit::migrate_framework`]). There is no lazy `ensure_*`
//! creation layer — the tables exist because migrations ran, exactly like
//! the rest of the framework's own tables.

use crate::sql::{Auto, ExecError, Pool};

/// A tenant-scoped MCP agent. Authenticates with a `prefix.secret`
/// credential; the secret half is argon2id-hashed in `secret_hash` and
/// never stored or returned in the clear.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_agents",
    display = "name",
    admin(
        list_display = "name, user_id, active, secret_prefix, created_at",
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
    /// `rustango_users`, but no hard FK is declared — that would couple the
    /// `mcp` feature to `tenancy` and break mcp-without-tenancy builds. Orphan
    /// cleanup on user deletion is explicit via [`delete_user_keys_pool`].
    pub user_id: Option<i64>,
    /// Flexible per-agent metadata. Never read by the framework.
    #[rustango(default = "'{}'")]
    pub data: serde_json::Value,
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

/// Authenticate a full `prefix.secret` credential by its **secret prefix**
/// (the lookup half) rather than the agent name — the shape a bearer token
/// carries when the raw credential itself is presented (epic #1013), where the
/// client never knows the agent's name. Same fail-closed + timing-neutral
/// contract as [`authenticate_agent_pool`].
///
/// # Errors
/// Propagates DB errors only; an unverifiable secret is `Ok(None)`.
pub async fn authenticate_agent_by_prefix_pool(
    pool: &Pool,
    prefix: &str,
    secret: &str,
) -> Result<Option<Agent>, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    let Some(agent) = Agent::objects()
        .where_(Agent::secret_prefix.eq(prefix))
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .filter(|a| a.active)
    else {
        // Timing-neutral for unknown prefixes (#1099).
        super::password::verify_dummy(secret);
        return Ok(None);
    };

    match super::password::verify(secret, &agent.secret_hash) {
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

/// The skill ids the owning user is **entitled** to. `Ok(None)` means a
/// superuser (entitled to every skill); `Ok(Some(ids))` is the set of skills
/// mapped to a permission the user currently holds (may be empty).
///
/// # Errors
/// Propagates DB / permission-lookup errors.
async fn user_entitled_skill_ids(
    pool: &Pool,
    user_id: i64,
) -> Result<Option<Vec<i64>>, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    let is_superuser = crate::tenancy::User::objects()
        .filter("id", user_id)
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .is_some_and(|u| u.is_superuser);
    if is_superuser {
        return Ok(None);
    }
    let perms = super::user_permissions_pool(user_id, pool).await?;
    if perms.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mapped: Vec<AgentSkillPermission> = AgentSkillPermission::objects()
        .where_(AgentSkillPermission::permission_codename.is_in(perms))
        .fetch(pool)
        .await?;
    let mut ids: Vec<i64> = Vec::new();
    for m in mapped {
        if !ids.contains(&m.skill_id) {
            ids.push(m.skill_id);
        }
    }
    Ok(Some(ids))
}

/// Resolve a **user-owned** agent's effective `(skill codenames, tools)`,
/// along two axes and always bounded by the owner's entitlement:
///
/// * **Entitlement** — which skills the owner may use: *every* skill for a
///   superuser, otherwise the skills mapped (via [`AgentSkillPermission`]) to a
///   permission the owner currently holds.
/// * **Scope** — if the key was created with pinned skills ([`AgentGrant`]s via
///   [`create_user_key_pool`]) it is limited to those; an unscoped key gets the
///   owner's full entitlement.
///
/// Effective skills = `pinned ∩ entitled` for a scoped key, or the full
/// entitlement for an unscoped one. A key can therefore never exceed the
/// owner's permissions, and revoking a permission narrows every key at the next
/// mint. [`crate::mcp`] bakes the resulting `tools` into the scoped JWT.
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

    let entitled = user_entitled_skill_ids(pool, user_id).await?;

    // Skills pinned onto THIS key at creation (the per-key scope).
    let pinned: Vec<i64> = {
        let grants: Vec<AgentGrant> = AgentGrant::objects()
            .filter("agent_id", agent_id)
            .fetch(pool)
            .await?;
        let mut ids = Vec::new();
        for g in grants {
            if !ids.contains(&g.skill_id) {
                ids.push(g.skill_id);
            }
        }
        ids
    };

    // Effective skill ids: pinned∩entitled (scoped) or the full entitlement.
    let effective: Vec<i64> = match (&entitled, pinned.is_empty()) {
        (None, true) => AgentSkill::objects()
            .fetch(pool)
            .await?
            .into_iter()
            .filter_map(|s| s.id.get().copied())
            .collect(), // superuser, unscoped → every skill
        (None, false) => pinned, // superuser, scoped → the pinned skills
        (Some(ent), true) => ent.clone(), // non-superuser, unscoped → entitled
        (Some(ent), false) => pinned.into_iter().filter(|id| ent.contains(id)).collect(),
    };
    if effective.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let skills: Vec<String> = AgentSkill::objects()
        .where_(AgentSkill::id.is_in(effective.clone()))
        .fetch(pool)
        .await?
        .into_iter()
        .map(|s| s.codename)
        .collect();
    let mut tools: Vec<String> = Vec::new();
    for st in AgentSkillTool::objects()
        .where_(AgentSkillTool::skill_id.is_in(effective))
        .fetch(pool)
        .await?
    {
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
/// (`prefix.secret`) is shown once and never recoverable.
///
/// `skills` sets the key's scope:
/// * **empty** → an *unscoped* key that can use everything the owner is
///   entitled to (their full permission-derived skill set, or all skills for a
///   superuser).
/// * **non-empty** → a *scoped* key limited to exactly those skill codenames
///   (a single skill or a skillset). Each must be a real skill the owner is
///   entitled to; otherwise this fails — you can't mint a key more capable than
///   yourself. Scoping pins [`AgentGrant`]s, and resolution always re-intersects
///   with the owner's current entitlement at mint
///   ([`resolve_user_agent_grants_pool`]).
///
/// # Errors
/// [`AgentError::NotFound`] for an unknown skill codename;
/// [`AgentError::Tenancy`] if the owner isn't entitled to a requested skill;
/// a DB / secret-generation error otherwise.
pub async fn create_user_key_pool(
    pool: &Pool,
    user_id: i64,
    label: &str,
    skills: &[String],
) -> Result<AgentSecret, AgentError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    // Resolve + entitlement-check every requested skill up front, so an invalid
    // request fails before any key row is written.
    let mut skill_ids: Vec<i64> = Vec::new();
    if !skills.is_empty() {
        let entitled = user_entitled_skill_ids(pool, user_id).await?;
        for codename in skills {
            let skill = AgentSkill::objects()
                .where_(AgentSkill::codename.eq(codename.clone()))
                .limit(1)
                .fetch(pool)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| AgentError::NotFound(codename.clone()))?;
            let sid = skill.id.get().copied().unwrap_or_default();
            let entitled_to_it = match &entitled {
                None => true, // superuser
                Some(ids) => ids.contains(&sid),
            };
            if !entitled_to_it {
                return Err(AgentError::Tenancy(super::error::TenancyError::Validation(
                    format!("owner is not entitled to skill `{codename}`"),
                )));
            }
            if !skill_ids.contains(&sid) {
                skill_ids.push(sid);
            }
        }
    }

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
    let agent_id = agent.id.get().copied().unwrap_or_default();

    // Pin the scoped skills as grants (the per-key scope).
    for sid in skill_ids {
        let mut grant = AgentGrant {
            id: Auto::default(),
            agent_id,
            skill_id: sid,
            data: serde_json::json!({}),
        };
        grant.insert_pool(pool).await?;
    }
    Ok(AgentSecret { agent, token })
}

/// List a user's personal keys, newest first.
///
/// # Errors
/// Propagates DB errors.
pub async fn list_user_keys_pool(pool: &Pool, user_id: i64) -> Result<Vec<Agent>, AgentError> {
    use crate::sql::FetcherPool as _;
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

/// Delete **every** user-owned key belonging to `user_id` — all [`Agent`]
/// rows with `user_id == Some(user_id)` — removing each agent's
/// [`AgentGrant`]s first (not relying on FK cascade, which some SQLite
/// connections don't enforce). Call this from a tenant user-deletion path so a
/// departed member's personal MCP keys can't outlive them. There is no hard FK
/// from `rustango_agents.user_id` to `rustango_users` (it would couple `mcp` →
/// `tenancy`), so orphan cleanup is explicit.
///
/// # Errors
/// Propagates DB errors.
pub async fn delete_user_keys_pool(pool: &Pool, user_id: i64) -> Result<(), AgentError> {
    use crate::sql::FetcherPool as _;

    let agents: Vec<Agent> = Agent::objects()
        .filter("user_id", user_id)
        .fetch(pool)
        .await?;
    for agent in agents {
        let agent_id = agent.id.get().copied().unwrap_or_default();
        let grants: Vec<AgentGrant> = AgentGrant::objects()
            .filter("agent_id", agent_id)
            .fetch(pool)
            .await?;
        for g in grants {
            g.delete_pool(pool).await?;
        }
        agent.delete_pool(pool).await?;
    }
    Ok(())
}

/// Request-time liveness re-check for a verified agent token. MCP agent JWTs
/// are stateless — [`crate::mcp::verify_agent_token`] proves the token is
/// well-formed and tenant-pinned but not that the agent still exists. Call
/// this right after verification (where the tenant [`Pool`] is available) so
/// that revoking / deactivating a key takes effect immediately instead of
/// lingering until the token expires.
///
/// Returns `false` (reject) when the agent row is absent or `active == false`,
/// and — for a user-owned key (`user_id.is_some()`) — when the owning user is
/// missing or `active == false`. One cheap lookup per request.
///
/// # Errors
/// Propagates DB errors.
pub async fn agent_token_still_valid_pool(
    pool: &Pool,
    agent_id: i64,
    user_id: Option<i64>,
) -> Result<bool, AgentError> {
    use crate::sql::FetcherPool as _;

    let Some(agent) = Agent::objects()
        .filter("id", agent_id)
        .limit(1)
        .fetch(pool)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(false);
    };
    if !agent.active {
        return Ok(false);
    }

    if let Some(uid) = user_id {
        let owner_active = crate::tenancy::User::objects()
            .filter("id", uid)
            .limit(1)
            .fetch(pool)
            .await?
            .into_iter()
            .next()
            .is_some_and(|u| u.active);
        if !owner_active {
            return Ok(false);
        }
    }
    Ok(true)
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
