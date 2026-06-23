//! `manage` verbs for MCP agents (epic #1013, Slice 2 / #1015):
//! `create-agent`, `rotate-agent-secret`, `list-agents`. Tenant-scoped —
//! each takes a `<slug>` and operates on that tenant's pool, mirroring the
//! `create-user` verb. The generated secret is printed exactly once.

use std::io::Write;

use sqlx::Database;

use crate::tenancy::error::TenancyError;
use crate::tenancy::manage::args::{next_value, reject_leading_flag};
use crate::tenancy::pools::TenantPools;

use super::users::scoped_tenant_pool;

const CREATE_HELP: &str = "create-agent <slug> <name>";
const ROTATE_HELP: &str = "rotate-agent-secret <slug> <name>";
const LIST_HELP: &str = "list-agents <slug>";
const CREATE_SKILL_HELP: &str =
    "create-skill <slug> <codename> [--name <s>] [--description <s>] [--tools t1,t2] [--instructions <s>]";
const GRANT_HELP: &str = "grant-skill <slug> <agent> <skill>";
const REVOKE_HELP: &str = "revoke-skill <slug> <agent> <skill>";
const LIST_SKILLS_HELP: &str = "list-skills <slug>";

/// `create-agent <slug> <name>` — provision a new MCP agent in the tenant
/// and print its one-time `prefix.secret` credential.
pub(super) async fn create_agent_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(args, "create-agent", "slug", CREATE_HELP)?;
    let mut iter = args.iter();
    let slug = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(CREATE_HELP.into()))?;
    let name = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(CREATE_HELP.into()))?;
    if let Some(extra) = iter.next() {
        return Err(TenancyError::Validation(format!(
            "create-agent: unexpected argument `{extra}`"
        )));
    }

    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;
    let issued = crate::tenancy::create_agent_pool(&scoped, &name)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;
    let id = issued.agent.id.get().copied().unwrap_or_default();
    writeln!(w, "created agent `{name}` (id {id}) in tenant `{slug}`")?;
    writeln!(w, "  secret: {}", issued.token)?;
    writeln!(w, "  store this safely — it won't be shown again")?;
    Ok(())
}

/// `rotate-agent-secret <slug> <name>` — issue a fresh secret for an
/// existing agent, invalidating the old one. Prints the new credential.
pub(super) async fn rotate_agent_secret_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(args, "rotate-agent-secret", "slug", ROTATE_HELP)?;
    let mut iter = args.iter();
    let slug = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(ROTATE_HELP.into()))?;
    let name = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(ROTATE_HELP.into()))?;

    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;
    let issued = crate::tenancy::rotate_agent_secret_pool(&scoped, &name)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;
    writeln!(w, "rotated secret for agent `{name}` in tenant `{slug}`")?;
    writeln!(w, "  new secret: {}", issued.token)?;
    writeln!(w, "  the previous secret no longer authenticates")?;
    Ok(())
}

/// `list-agents <slug>` — print every agent in the tenant.
pub(super) async fn list_agents_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(args, "list-agents", "slug", LIST_HELP)?;
    let slug = args
        .first()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(LIST_HELP.into()))?;

    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;
    let agents = crate::tenancy::list_agents_pool(&scoped)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;
    if agents.is_empty() {
        writeln!(w, "no agents in tenant `{slug}`")?;
        return Ok(());
    }
    writeln!(w, "agents in tenant `{slug}`:")?;
    for a in &agents {
        let id = a.id.get().copied().unwrap_or_default();
        let status = if a.active { "active" } else { "disabled" };
        writeln!(
            w,
            "  {id:>4}  {}  ({status}, prefix {})",
            a.name, a.secret_prefix
        )?;
    }
    Ok(())
}

/// `create-skill <slug> <codename> [--name ..] [--description ..] [--tools a,b] [--instructions ..]`
pub(super) async fn create_skill_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(args, "create-skill", "slug", CREATE_SKILL_HELP)?;
    let mut iter = args.iter();
    let slug = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(CREATE_SKILL_HELP.into()))?;
    let codename = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(CREATE_SKILL_HELP.into()))?;
    let mut name = String::new();
    let mut description = String::new();
    let mut instructions = String::new();
    let mut tools: Vec<String> = Vec::new();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--name" => name = next_value(&mut iter, "--name")?,
            "--description" => description = next_value(&mut iter, "--description")?,
            "--instructions" => instructions = next_value(&mut iter, "--instructions")?,
            "--tools" => {
                tools = next_value(&mut iter, "--tools")?
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-skill: unknown argument `{other}`"
                )));
            }
        }
    }

    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;
    let skill = crate::tenancy::create_skill_pool(
        &scoped,
        &codename,
        &name,
        &description,
        &instructions,
        &tools,
    )
    .await
    .map_err(|e| TenancyError::Validation(e.to_string()))?;
    let id = skill.id.get().copied().unwrap_or_default();
    writeln!(
        w,
        "created skill `{codename}` (id {id}) in tenant `{slug}` with {} tool(s)",
        tools.len()
    )?;
    Ok(())
}

/// `grant-skill <slug> <agent> <skill>`
pub(super) async fn grant_skill_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, agent, skill) = three_positionals(args, "grant-skill", GRANT_HELP)?;
    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;
    crate::tenancy::grant_skill_pool(&scoped, &agent, &skill)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;
    writeln!(
        w,
        "granted skill `{skill}` to agent `{agent}` in tenant `{slug}`"
    )?;
    Ok(())
}

/// `revoke-skill <slug> <agent> <skill>`
pub(super) async fn revoke_skill_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, agent, skill) = three_positionals(args, "revoke-skill", REVOKE_HELP)?;
    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;
    crate::tenancy::revoke_skill_pool(&scoped, &agent, &skill)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;
    writeln!(
        w,
        "revoked skill `{skill}` from agent `{agent}` in tenant `{slug}`"
    )?;
    Ok(())
}

/// `list-skills <slug>`
pub(super) async fn list_skills_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(args, "list-skills", "slug", LIST_SKILLS_HELP)?;
    let slug = args
        .first()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(LIST_SKILLS_HELP.into()))?;
    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;
    let skills = crate::tenancy::list_skills_pool(&scoped)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;
    if skills.is_empty() {
        writeln!(w, "no skills in tenant `{slug}`")?;
        return Ok(());
    }
    writeln!(w, "skills in tenant `{slug}`:")?;
    for s in &skills {
        writeln!(w, "  {}  {}", s.codename, s.name)?;
    }
    Ok(())
}

/// Parse exactly three positional args (`<slug> <a> <b>`) for the grant verbs.
fn three_positionals(
    args: &[String],
    verb: &str,
    help: &str,
) -> Result<(String, String, String), TenancyError> {
    reject_leading_flag(args, verb, "slug", help)?;
    match args {
        [slug, a, b] => Ok((slug.clone(), a.clone(), b.clone())),
        _ => Err(TenancyError::Validation(help.into())),
    }
}
