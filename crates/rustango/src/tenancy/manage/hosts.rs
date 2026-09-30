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

use super::args::{parse, Parsed, Spec};

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

/// Record a direction, refusing a second one that disagrees.
///
/// Last-wins is a guess, and the guess is exactly what these verbs refuse
/// to make: `--on --off` used to park a live host and report success
/// (#1355). Repeating the *same* direction is harmless and allowed.
pub(super) fn set_direction(
    slot: &mut Option<bool>,
    value: bool,
    pair: (&str, &str),
) -> Result<(), TenancyError> {
    match slot {
        Some(prev) if *prev != value => Err(TenancyError::Validation(format!(
            "{} and {} contradict each other — pass one",
            pair.0, pair.1
        ))),
        _ => {
            *slot = Some(value);
            Ok(())
        }
    }
}

/// A closed set, case-insensitively. Anything else is refused rather than
/// read as `false`, which is how `--enabled TRUE` used to park a host.
fn parse_bool(raw: &str) -> Result<bool, TenancyError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(TenancyError::Validation(format!(
            "`{other}` is not a yes/no value — use --on or --off"
        ))),
    }
}

/// `<slug> <hostname>` plus no flags — the shape of add- and remove-host.
fn slug_host_spec(verb: &str) -> Spec<'_> {
    Spec {
        verb,
        usage: "<verb> <slug> <hostname>",
        switches: &[],
        valued: &[],
        max_positionals: 2,
    }
}

/// The slug and hostname every host verb needs. Flags may sit anywhere,
/// and a flag's value is never one of them (#1910).
fn slug_and_host(parsed: &Parsed, verb: &str) -> Result<(String, String), TenancyError> {
    let slug = positional_or_ask(
        parsed.positional(0),
        "Tenant slug: ",
        &format!("{verb} requires a tenant slug"),
    )?;
    let host = positional_or_ask(
        parsed.positional(1),
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
    let parsed = parse(
        args,
        &Spec {
            verb: "list-hosts",
            usage: "list-hosts <slug>",
            switches: &[],
            valued: &[],
            max_positionals: 1,
        },
    )?;
    let slug = positional_or_ask(
        parsed.positional(0),
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
    let parsed = parse(args, &slug_host_spec("add-host"))?;
    let (slug, host) = slug_and_host(&parsed, "add-host")?;
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
    let parsed = parse(args, &slug_host_spec("remove-host"))?;
    let (slug, host) = slug_and_host(&parsed, "remove-host")?;
    org_host::remove_host(&pools.registry_pool(), &slug, &host)
        .await
        .map_err(explain)?;
    writeln!(w, "unbound {host} from {slug}")?;
    Ok(())
}

const SET_HOST_ENABLED: Spec<'static> = Spec {
    verb: "set-host-enabled",
    usage: "set-host-enabled <slug> <hostname> --on|--off",
    switches: &["--on", "--off"],
    valued: &["--enabled"],
    max_positionals: 2,
};

pub(super) async fn set_host_enabled<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let parsed = parse(args, &SET_HOST_ENABLED)?;
    let (slug, host) = slug_and_host(&parsed, "set-host-enabled")?;

    let mut enabled: Option<bool> = None;
    let pair = ("--on", "--off");
    if parsed.has("--on") {
        set_direction(&mut enabled, true, pair)?;
    }
    if parsed.has("--off") {
        set_direction(&mut enabled, false, pair)?;
    }
    if let Some(raw) = parsed.value("--enabled")? {
        set_direction(&mut enabled, parse_bool(raw)?, pair)?;
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

    fn add(args: &[&str]) -> Result<(String, String), TenancyError> {
        let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        slug_and_host(&parse(&argv, &slug_host_spec("add-host"))?, "add-host")
    }

    #[test]
    fn a_missing_slug_or_host_is_named_in_the_error() {
        let err = add(&[]).expect_err("no slug");
        assert!(err.to_string().contains("slug"), "{err}");

        let err = add(&["acme"]).expect_err("no host");
        assert!(err.to_string().contains("hostname"), "{err}");

        assert_eq!(
            add(&["acme", "shop.test"]).expect("ok"),
            ("acme".to_owned(), "shop.test".to_owned())
        );
    }

    /// Flags may precede or follow the positionals without being mistaken
    /// for one — `set-host-enabled acme x.test --off` and
    /// `set-host-enabled --off acme x.test` mean the same thing.
    #[test]
    fn flags_are_not_mistaken_for_positionals() {
        for args in [
            &["--off", "acme", "shop.test"][..],
            &["--enabled", "false", "acme", "shop.test"],
        ] {
            let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            let parsed = parse(&argv, &SET_HOST_ENABLED).expect("parse");
            assert_eq!(
                slug_and_host(&parsed, "set-host-enabled").expect("ok"),
                ("acme".to_owned(), "shop.test".to_owned()),
                "{args:?}"
            );
        }
    }
}
