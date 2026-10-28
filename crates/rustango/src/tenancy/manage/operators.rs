//! Operator inspection and activation — `list-operators`,
//! `set-operator-active`.
//!
//! `create-operator` and `reset-operator-password` already existed; seeing
//! who exists and turning one off did not, so an operator who left the
//! company could only be disabled through the console (#1344).
//!
//! Both verbs go through [`crate::tenancy::operators`], the engine the
//! console calls, so the lockout rules are enforced once rather than
//! restated here.

use std::io::Write;

use sqlx::Database;

use crate::manage_interactive;
use crate::tenancy::error::TenancyError;
use crate::tenancy::operators::{self as ops, Outcome};
use crate::tenancy::pools::TenantPools;

fn explain(e: ops::OperatorError) -> TenancyError {
    match e {
        ops::OperatorError::Driver(d) => TenancyError::from(d),
        other => TenancyError::Validation(other.to_string()),
    }
}

pub(super) async fn list_operators<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    super::args::no_args(args, "list-operators")?;
    let rows = ops::list(&pools.registry_pool()).await.map_err(explain)?;
    if rows.is_empty() {
        writeln!(w, "(no operators — create one with `create-operator`)")?;
        return Ok(());
    }
    writeln!(w, "{:<32} {:<8} created_at", "username", "active")?;
    writeln!(w, "{}", "-".repeat(64))?;
    for o in &rows {
        writeln!(
            w,
            "{:<32} {:<8} {}",
            o.username,
            o.active,
            o.created_at.format("%Y-%m-%d %H:%M:%SZ"),
        )?;
    }
    let active = rows.iter().filter(|o| o.active).count();
    writeln!(w, "\n{} operator(s), {active} active", rows.len())?;
    Ok(())
}

pub(super) async fn set_operator_active<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // A second positional was once ignored (#1952).
    let parsed = super::args::parse(
        args,
        &super::args::Spec {
            verb: "set-operator-active",
            usage: "set-operator-active <username> --on|--off",
            switches: &["--on", "--off"],
            valued: &[],
            max_positionals: 1,
        },
    )?;
    // `--on --off` is refused rather than last-wins: guessing revokes or
    // restores access, and both are wrong to do silently (#1355).
    let active = parsed.on_off()?;
    let username = match parsed.positional(0) {
        Some(u) => u.clone(),
        None => manage_interactive::ask("Operator username: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("set-operator-active requires a username".into())
            })?,
    };
    let active = active.ok_or_else(|| {
        TenancyError::Validation("set-operator-active requires --on or --off".into())
    })?;

    let registry = pools.registry_pool();
    let mut target = ops::by_username(&registry, &username)
        .await
        .map_err(explain)?;

    // `actor: None` — a shell has no session to lock itself out of. The
    // last-active rule still applies: it is the invariant, and `--on` is
    // always allowed, so the CLI can never paint itself into a corner.
    match ops::set_active(&registry, &mut target, active, None)
        .await
        .map_err(explain)?
    {
        Outcome::AlreadySo => writeln!(
            w,
            "`{username}` was already {}",
            if active { "active" } else { "inactive" }
        )?,
        Outcome::Changed => writeln!(
            w,
            "{} `{username}`",
            if active { "activated" } else { "deactivated" }
        )?,
    }
    Ok(())
}

/// `reset-operator-totp <username>`: drop the operator's authenticator, for
/// a lost device. Audited on the registry with `source = cli`.
#[cfg(feature = "totp")]
pub(super) async fn reset_operator_totp<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::audit::{AuditOp, AuditSource, PendingEntry};
    use crate::tenancy::operator_totp::{self, OperatorTotp};
    const USAGE: &str = "reset-operator-totp <username>";
    let parsed = super::args::parse(
        args,
        &super::args::Spec {
            verb: "reset-operator-totp",
            usage: USAGE,
            switches: &[],
            valued: &[],
            max_positionals: 1,
        },
    )?;
    let username = parsed.required(0, USAGE)?;
    let registry = pools.registry_pool();
    let op = ops::by_username(&registry, &username)
        .await
        .map_err(explain)?;
    let id = op.id.get().copied().unwrap_or_default();
    operator_totp::ensure_table(&registry).await?;
    if crate::admin::totp_store::reset_in::<OperatorTotp>(&registry, id).await? == 0 {
        writeln!(w, "`{username}` has no two-factor device")?;
        return Ok(());
    }
    crate::audit::ensure_table_pool(&registry).await?;
    let entry = PendingEntry {
        entity_table: "rustango_operators",
        entity_pk: id.to_string(),
        operation: AuditOp::Action,
        source: AuditSource::Custom("cli".into()),
        changes: serde_json::json!({ "action": "totp_reset" }),
    };
    crate::audit::emit_one_pool(&registry, &entry).await?;
    writeln!(w, "two-factor device removed for operator `{username}`")?;
    Ok(())
}
