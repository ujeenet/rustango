//! Hostname verbs — `list-hosts`, `add-host`, `remove-host`,
//! `set-host-enabled`.
//!
//! The operator console has bound extra hostnames since #1318; the CLI could
//! not, so the action could not run in a deploy hook or on a box with no
//! browser (#1344). These are argv over [`crate::tenancy::org_host`], the
//! same engine the console posts to.
//!
//! No `rename-host`: [`org_host::generation`] fingerprints the table by row
//! count, enabled count, max id and enabled-id sum, so an in-place rename
//! would not move it and other pods would keep routing the old name until
//! the TTL expired. Remove and add instead.

use std::io::Write;

use sqlx::Database;

use crate::manage_interactive;
use crate::tenancy::error::TenancyError;
use crate::tenancy::org_host::{self, HostError};
use crate::tenancy::pools::TenantPools;

use super::args::next_value;

/// Host errors carry the operator-facing wording already; keep it.
fn explain(e: HostError) -> TenancyError {
    match e {
        HostError::Driver(d) => TenancyError::from(d),
        other => TenancyError::Validation(other.to_string()),
    }
}

/// A positional argument, or the answer to a prompt on a terminal.
///
/// Same shape as `create-tenant` and `create-operator`: scripted callers see
/// the validation error they always did, and a person gets asked. It is also
/// what lets the menu offer these verbs without re-listing their positionals.
fn positional_or_ask(
    given: Option<&String>,
    prompt: &str,
    missing: &str,
) -> Result<String, TenancyError> {
    match given {
        Some(v) => Ok(v.clone()),
        None => manage_interactive::ask(prompt)
            .map_err(TenancyError::Io)?
            .ok_or_else(|| TenancyError::Validation(missing.to_owned())),
    }
}

/// The slug and hostname every host verb needs. Flags may sit anywhere.
fn slug_and_host(args: &[String], verb: &str) -> Result<(String, String), TenancyError> {
    let mut positional = args.iter().filter(|a| !a.starts_with("--"));
    let slug = positional_or_ask(
        positional.next(),
        "Tenant slug: ",
        &format!("{verb} requires a tenant slug"),
    )?;
    let host = positional_or_ask(
        positional.next(),
        "Hostname: ",
        &format!("{verb} requires a hostname"),
    )?;
    Ok((slug, host))
}

pub(super) async fn list_hosts<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let slug = positional_or_ask(
        args.iter().find(|a| !a.starts_with("--")),
        "Tenant slug: ",
        "list-hosts requires a tenant slug",
    )?;

    let hosts = org_host::list_for_org(&pools.registry_pool(), &slug)
        .await
        .map_err(explain)?;
    if hosts.is_empty() {
        writeln!(
            w,
            "(no hostnames — `{slug}` is reachable by subdomain only)"
        )?;
        return Ok(());
    }
    writeln!(w, "{:<48} {:<8} source", "hostname", "enabled")?;
    writeln!(w, "{}", "-".repeat(70))?;
    for h in &hosts {
        writeln!(
            w,
            "{:<48} {:<8} {}",
            h.hostname,
            h.enabled,
            if h.is_base { "base" } else { "extra" }
        )?;
    }
    Ok(())
}

pub(super) async fn add_host<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, host) = slug_and_host(args, "add-host")?;
    let row = org_host::add_host(&pools.registry_pool(), &slug, &host)
        .await
        .map_err(explain)?;
    // The engine normalizes; echo what was stored, not what was typed.
    writeln!(w, "bound {} -> {slug}", row.hostname)?;
    if !row.enabled {
        writeln!(w, "  (parked — enable with `set-host-enabled`)")?;
    }
    Ok(())
}

pub(super) async fn remove_host<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, host) = slug_and_host(args, "remove-host")?;
    org_host::remove_host(&pools.registry_pool(), &slug, &host)
        .await
        .map_err(explain)?;
    writeln!(w, "unbound {host} from {slug}")?;
    Ok(())
}

pub(super) async fn set_host_enabled<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, host) = slug_and_host(args, "set-host-enabled")?;

    let mut enabled: Option<bool> = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--on" => enabled = Some(true),
            "--off" => enabled = Some(false),
            "--enabled" => {
                let raw = next_value(&mut iter, "--enabled")?;
                enabled = Some(matches!(raw.as_str(), "1" | "true" | "yes" | "on"));
            }
            other if other.starts_with("--") => {
                return Err(TenancyError::Validation(format!(
                    "unknown flag `{other}` — set-host-enabled takes --on or --off"
                )))
            }
            _ => {}
        }
    }
    // No default: "which way?" has no safe guess, and silently picking one
    // would park a live host or serve a parked one.
    let enabled = enabled.ok_or_else(|| {
        TenancyError::Validation("set-host-enabled requires --on or --off".into())
    })?;

    org_host::set_host_enabled(&pools.registry_pool(), &slug, &host, enabled)
        .await
        .map_err(explain)?;
    writeln!(
        w,
        "{host} is now {} for {slug}",
        if enabled { "served" } else { "parked" }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_slug_or_host_is_named_in_the_error() {
        let none: Vec<String> = Vec::new();
        let err = slug_and_host(&none, "add-host").expect_err("no slug");
        assert!(err.to_string().contains("slug"), "{err}");

        let only_slug = vec!["acme".to_owned()];
        let err = slug_and_host(&only_slug, "add-host").expect_err("no host");
        assert!(err.to_string().contains("hostname"), "{err}");

        let both = vec!["acme".to_owned(), "shop.test".to_owned()];
        assert_eq!(
            slug_and_host(&both, "add-host").expect("ok"),
            ("acme".to_owned(), "shop.test".to_owned())
        );
    }

    /// Flags may precede or follow the positionals without being mistaken
    /// for one — `set-host-enabled acme x.test --off` and
    /// `set-host-enabled --off acme x.test` mean the same thing.
    #[test]
    fn flags_are_not_mistaken_for_positionals() {
        let args = vec![
            "--off".to_owned(),
            "acme".to_owned(),
            "shop.test".to_owned(),
        ];
        assert_eq!(
            slug_and_host(&args, "set-host-enabled").expect("ok"),
            ("acme".to_owned(), "shop.test".to_owned())
        );
    }
}
