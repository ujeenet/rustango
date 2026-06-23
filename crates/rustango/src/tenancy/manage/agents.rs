//! `manage` verbs for MCP agents (epic #1013, Slice 2 / #1015):
//! `create-agent`, `rotate-agent-secret`, `list-agents`. Tenant-scoped —
//! each takes a `<slug>` and operates on that tenant's pool, mirroring the
//! `create-user` verb. The generated secret is printed exactly once.

use std::io::Write;

use sqlx::Database;

use crate::tenancy::error::TenancyError;
use crate::tenancy::manage::args::reject_leading_flag;
use crate::tenancy::pools::TenantPools;

use super::users::scoped_tenant_pool;

const CREATE_HELP: &str = "create-agent <slug> <name>";
const ROTATE_HELP: &str = "rotate-agent-secret <slug> <name>";
const LIST_HELP: &str = "list-agents <slug>";

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
