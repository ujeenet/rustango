//! `audit-cleanup` management verb — runs audit-log retention across
//! the registry's own log and every active tenant's.
//!
//! The registry keeps an audit log too: every operator action the
//! console records — tenant edits, hostname changes, operator
//! management, purges. This verb only ever walked tenants, so nothing
//! trimmed it and it grew with console use.
//!
//! Two retention modes, mutually exclusive:
//!
//! * `--days <N>`        — delete entries older than N days.
//! * `--keep-last <N>`   — keep the N most recent entries per
//!   `(entity_table, entity_pk)` pair; delete the rest.
//!
//! ```text
//! cargo run -- audit-cleanup --days 90
//! cargo run -- audit-cleanup --keep-last 50
//! cargo run -- audit-cleanup --tenant acme --days 90
//! ```
//!
//! An `N` of 0 or less deletes every entry, so it needs `--yes`.

use std::io::Write;

use sqlx::Database;

use crate::core::Column as _;
use crate::sql::FetcherPool as _;
use crate::tenancy::error::TenancyError;
use crate::tenancy::pools::TenantPools;
use crate::tenancy::Org;

use super::args::next_value;

pub(super) async fn audit_cleanup_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut days: Option<i64> = None;
    let mut keep_last: Option<i64> = None;
    let mut tenant_slug: Option<String> = None;
    let mut registry_only = false;
    let mut yes = false;

    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--days" => {
                let raw = next_value(&mut iter, "--days")?;
                days = Some(raw.parse::<i64>().map_err(|_| {
                    TenancyError::Validation(format!("--days expects an integer, got `{raw}`"))
                })?);
            }
            "--keep-last" => {
                let raw = next_value(&mut iter, "--keep-last")?;
                keep_last = Some(raw.parse::<i64>().map_err(|_| {
                    TenancyError::Validation(format!("--keep-last expects an integer, got `{raw}`"))
                })?);
            }
            "--tenant" => {
                tenant_slug = Some(next_value(&mut iter, "--tenant")?);
            }
            "--registry" => registry_only = true,
            "--yes" => yes = true,
            "--help" | "-h" => {
                write_verb_help(w)?;
                return Ok(());
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "unknown flag `{other}` — run with --help for usage"
                )))
            }
        }
    }

    let configured = if days.is_none() && keep_last.is_none() {
        crate::audit::Retention::configured_days().map_err(TenancyError::Validation)?
    } else {
        None
    };
    let rule = crate::audit::Retention::from_flags(days, keep_last, yes, configured)
        .map_err(TenancyError::Validation)?;

    if registry_only && tenant_slug.is_some() {
        return Err(TenancyError::Validation(
            "--registry and --tenant name different logs — pass one".into(),
        ));
    }

    let registry = pools.registry_pool();
    let mut total_deleted: u64 = 0;
    let mut swept = 0usize;
    let mut failed = 0usize;

    // The registry keeps an audit log of its own — every operator
    // action the console records — and this verb only ever walked
    // tenants, so nothing trimmed it. Swept unless a single tenant was
    // named, which asks for that tenant and nothing else.
    if tenant_slug.is_none() {
        let deleted = rule.prune(&registry).await?;
        writeln!(w, "  registry deleted={deleted}")?;
        total_deleted += deleted;
    }

    if let Some(ref slug) = tenant_slug {
        let found: Vec<Org> = Org::objects()
            .where_(Org::slug.eq(slug.as_str()))
            .fetch(&registry)
            .await?;
        let org = found
            .into_iter()
            .next()
            .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` not found")))?;
        // One named tenant: a failure is the answer to what was asked,
        // so it propagates rather than being collected.
        let scoped = pools.scoped_pool_dyn(&org).await?;
        let deleted = rule.prune(&scoped).await?;
        writeln!(w, "  tenant={slug} deleted={deleted}")?;
        total_deleted += deleted;
        swept = 1;
    } else if !registry_only {
        // `for_each_tenant` resolves each pool the right way per storage
        // mode and — the reason for using it here — collects outcomes
        // instead of aborting. A single tenant whose schema is missing
        // used to end the sweep with a raw SQL error, leaving every
        // later tenant untrimmed.
        let sweep = super::super::sweep::for_each_tenant(pools, |_org, pool| async move {
            rule.prune(&pool).await.map_err(TenancyError::from)
        })
        .await?;

        for outcome in &sweep.outcomes {
            match &outcome.result {
                Ok(deleted) => {
                    writeln!(w, "  tenant={} deleted={}", outcome.slug, deleted)?;
                    total_deleted += *deleted;
                    swept += 1;
                }
                Err(e) => {
                    writeln!(w, "  tenant={} FAILED: {e}", outcome.slug)?;
                    failed += 1;
                }
            }
        }
    }

    writeln!(
        w,
        "audit-cleanup done: tenants={swept} failed={failed} total_deleted={total_deleted}"
    )?;
    super::migrations::tenant_failures(failed, swept + failed)
}

fn write_verb_help<W: Write>(w: &mut W) -> Result<(), TenancyError> {
    writeln!(
        w,
        "audit-cleanup — remove old entries from the registry's audit log and each tenant's"
    )?;
    writeln!(w)?;
    writeln!(w, "USAGE:")?;
    writeln!(
        w,
        "  audit-cleanup --days <N>           delete entries older than N days"
    )?;
    writeln!(
        w,
        "  audit-cleanup --keep-last <N>      keep N most recent entries per row"
    )?;
    writeln!(
        w,
        "  audit-cleanup                      neither flag: delete older than [audit] retention_days"
    )?;
    writeln!(w)?;
    writeln!(w, "OPTIONS:")?;
    writeln!(
        w,
        "  --tenant <slug>   that tenant's log only (default: the registry and every active tenant)"
    )?;
    writeln!(w, "  --registry        the registry's own log only")?;
    writeln!(
        w,
        "  --yes             confirm a 0 (or negative) N, which deletes every entry"
    )?;
    writeln!(w)?;
    writeln!(w, "EXAMPLES:")?;
    writeln!(w, "  cargo run -- audit-cleanup --days 90")?;
    writeln!(w, "  cargo run -- audit-cleanup --keep-last 50")?;
    writeln!(w, "  cargo run -- audit-cleanup --tenant acme --days 90")?;
    Ok(())
}
