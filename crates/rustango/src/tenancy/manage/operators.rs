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
    super::args::reject_extra_positionals(args, 0, "list-operators")?;
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
    let username = match args.iter().find(|a| !a.starts_with("--")) {
        Some(u) => u.clone(),
        None => manage_interactive::ask("Operator username: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("set-operator-active requires a username".into())
            })?,
    };

    let mut active: Option<bool> = None;
    for flag in args {
        match flag.as_str() {
            // Refused rather than last-wins: guessing revokes or restores
            // access, and both are wrong to do silently (#1355).
            "--on" => super::hosts::set_direction(&mut active, true, ("--on", "--off"))?,
            "--off" => super::hosts::set_direction(&mut active, false, ("--on", "--off"))?,
            other if other.starts_with("--") => {
                return Err(TenancyError::Validation(format!(
                    "unknown flag `{other}` — set-operator-active takes --on or --off"
                )))
            }
            _ => {}
        }
    }
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
