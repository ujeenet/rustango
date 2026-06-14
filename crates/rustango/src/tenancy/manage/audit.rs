//! `audit-cleanup` management verb — runs audit-log retention across
//! every active tenant (or a single named tenant with `--tenant`).
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

    match (days, keep_last) {
        (None, None) => {
            return Err(TenancyError::Validation(
                "audit-cleanup requires --days <N> or --keep-last <N>".into(),
            ))
        }
        (Some(_), Some(_)) => {
            return Err(TenancyError::Validation(
                "--days and --keep-last are mutually exclusive".into(),
            ))
        }
        _ => {}
    }

    let registry = pools.registry_pool();
    let orgs: Vec<Org> = if let Some(ref slug) = tenant_slug {
        let found: Vec<Org> = Org::objects()
            .where_(Org::slug.eq(slug.as_str()))
            .fetch(&registry)
            .await?;
        if found.is_empty() {
            return Err(TenancyError::Validation(format!(
                "tenant `{slug}` not found"
            )));
        }
        found
    } else {
        Org::objects()
            .where_(Org::active.eq(true))
            .fetch(&registry)
            .await?
    };

    let total_orgs = orgs.len();
    let mut total_deleted: u64 = 0;

    for org in &orgs {
        // v0.38 — route through scoped_pool_dyn which yields a
        // backend-agnostic `crate::sql::Pool` enum: on PG schema-mode
        // it builds a dedicated pool with `search_path` baked in;
        // database-mode (any backend) just wraps the cached pool.
        let scoped = pools.scoped_pool_dyn(org).await?;

        let deleted = if let Some(n) = days {
            crate::audit::cleanup_older_than_pool(&scoped, n).await?
        } else if let Some(n) = keep_last {
            crate::audit::cleanup_keep_last_n_pool(&scoped, n).await?
        } else {
            unreachable!()
        };

        writeln!(w, "  tenant={} deleted={}", org.slug, deleted)?;
        total_deleted += deleted;
    }

    writeln!(
        w,
        "audit-cleanup done: tenants={total_orgs} total_deleted={total_deleted}"
    )?;
    Ok(())
}

fn write_verb_help<W: Write>(w: &mut W) -> Result<(), TenancyError> {
    writeln!(
        w,
        "audit-cleanup — remove old entries from each tenant's audit log"
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
    writeln!(w)?;
    writeln!(w, "OPTIONS:")?;
    writeln!(
        w,
        "  --tenant <slug>   scope to one tenant (default: every active tenant)"
    )?;
    writeln!(w)?;
    writeln!(w, "EXAMPLES:")?;
    writeln!(w, "  cargo run -- audit-cleanup --days 90")?;
    writeln!(w, "  cargo run -- audit-cleanup --keep-last 50")?;
    writeln!(w, "  cargo run -- audit-cleanup --tenant acme --days 90")?;
    Ok(())
}
