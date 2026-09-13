//! Read-only inspection — `list-runs`, `show-run`, `audit-log` (#1344).
//!
//! The console renders all three. Nothing printed them, so the questions
//! asked during an incident — "did that provision finish?", "who
//! deactivated this tenant?" — needed a browser pointed at production, or
//! a SQL client on the registry.
//!
//! No new queries: `provision_store` and `crate::audit` already answer
//! these for the console pages, and these verbs render the same rows.

use std::io::Write;

use sqlx::Database;

use crate::tenancy::error::TenancyError;
use crate::tenancy::pools::TenantPools;
use crate::tenancy::provision_store;

use super::args::next_value;

/// Rows a page-less terminal can still scroll back through.
const DEFAULT_LIMIT: i64 = 20;

/// What `provision_store` writes into `kind` and `state`.
const KINDS: &[&str] = &["provision", "migrate"];
const STATES: &[&str] = &["pending", "running", "succeeded", "failed"];

/// Accept only a value the column can actually hold.
fn one_of(raw: &str, flag: &str, allowed: &[&str]) -> Result<String, TenancyError> {
    if allowed.contains(&raw) {
        return Ok(raw.to_owned());
    }
    Err(TenancyError::Validation(format!(
        "`{raw}` is not a {flag} value — try {}",
        allowed.join(", ")
    )))
}

fn parse_limit<'a, I: Iterator<Item = &'a String>>(
    iter: &mut I,
    flag: &str,
) -> Result<i64, TenancyError> {
    let raw = next_value(iter, flag)?;
    raw.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| TenancyError::Validation(format!("{flag} expects a positive integer")))
}

/// `Auto<DateTime>` is unset until the row round-trips; a run read back
/// always has one, so an empty cell means something is wrong, not missing.
fn stamp(t: &crate::sql::Auto<chrono::DateTime<chrono::Utc>>) -> String {
    t.get().map_or_else(
        || "-".to_owned(),
        |d| d.format("%Y-%m-%d %H:%M:%SZ").to_string(),
    )
}

/// Shorten for a column without hiding that it was shortened.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

pub(super) async fn list_runs<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut limit = DEFAULT_LIMIT;
    let mut kind: Option<String> = None;
    let mut state: Option<String> = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--limit" => limit = parse_limit(&mut iter, "--limit")?,
            // Validated, not passed through: a typo used to come back as
            // "(no runs)", which reads as "there are none" — the wrong
            // answer to "did that migration run?" (#1356).
            "--kind" => kind = Some(one_of(&next_value(&mut iter, "--kind")?, "--kind", KINDS)?),
            "--state" => {
                state = Some(one_of(
                    &next_value(&mut iter, "--state")?,
                    "--state",
                    STATES,
                )?);
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "unknown flag `{other}` — list-runs takes --limit, --kind, --state"
                )))
            }
        }
    }

    // Filtered in the query, not on the page: filtering the newest N would
    // report "none" whenever the newest N happen to be a different kind.
    let runs = provision_store::recent_runs_filtered(
        &pools.registry_pool(),
        kind.as_deref(),
        state.as_deref(),
        limit,
        0,
    )
    .await?;

    if runs.is_empty() {
        writeln!(w, "(no runs)")?;
        return Ok(());
    }
    writeln!(
        w,
        "{:<8} {:<10} {:<24} {:<10} started",
        "id", "kind", "slug", "state"
    )?;
    writeln!(w, "{}", "-".repeat(76))?;
    for r in &runs {
        let id = r.id.get().copied().unwrap_or_default();
        writeln!(
            w,
            "{:<8} {:<10} {:<24} {:<10} {}",
            id,
            clip(&r.kind, 10),
            // A batch migration stores an empty slug — it spans tenants.
            clip(if r.slug.is_empty() { "(all)" } else { &r.slug }, 24),
            clip(&r.state, 10),
            stamp(&r.started_at),
        )?;
    }
    writeln!(w, "\n`show-run <id>` for the steps of one")?;
    Ok(())
}

pub(super) async fn show_run<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let raw = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| TenancyError::Validation("show-run requires a run id".into()))?;
    let run_id = raw
        .parse::<i64>()
        .map_err(|_| TenancyError::Validation(format!("`{raw}` is not a run id")))?;

    let registry = pools.registry_pool();
    let run = provision_store::run_by_id(&registry, run_id)
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("no run {run_id}")))?;

    writeln!(w, "run {run_id}  {}  {}", run.kind, run.state)?;
    writeln!(
        w,
        "  tenant:     {}",
        if run.slug.is_empty() {
            "(every active tenant)"
        } else {
            &run.slug
        }
    )?;
    if !run.storage_mode.is_empty() {
        writeln!(
            w,
            "  storage:    {} / {}",
            run.storage_mode, run.backend_kind
        )?;
    }
    if let Some(by) = run.requested_by.as_deref() {
        writeln!(w, "  requested:  {by}")?;
    }
    writeln!(w, "  started:    {}", stamp(&run.started_at))?;
    if let Some(f) = run.finished_at {
        writeln!(w, "  finished:   {}", f.format("%Y-%m-%d %H:%M:%SZ"))?;
    }
    if let Some(e) = run.error.as_deref() {
        writeln!(w, "  error:      {e}")?;
    }

    // `0` means from the beginning — the same call the SSE stream makes to
    // replay a run for a client that reconnected.
    let events = provision_store::events_since(&registry, run_id, 0).await?;
    if events.is_empty() {
        writeln!(w, "\n(no steps recorded)")?;
        return Ok(());
    }
    writeln!(w, "\n{:<5} {:<22} {:<9} message", "seq", "step", "status")?;
    writeln!(w, "{}", "-".repeat(76))?;
    for e in &events {
        writeln!(
            w,
            "{:<5} {:<22} {:<9} {}",
            e.seq,
            clip(&e.step, 22),
            clip(&e.status, 9),
            e.message
        )?;
    }
    Ok(())
}

pub(super) async fn audit_log<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut limit = DEFAULT_LIMIT;
    let mut filter = crate::audit::AuditFilter {
        entity_table: None,
        entity_pk: None,
        operation: None,
        source: None,
    };
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--limit" => limit = parse_limit(&mut iter, "--limit")?,
            "--table" => filter.entity_table = Some(next_value(&mut iter, "--table")?),
            "--pk" => filter.entity_pk = Some(next_value(&mut iter, "--pk")?),
            "--operation" => filter.operation = Some(next_value(&mut iter, "--operation")?),
            "--source" => filter.source = Some(next_value(&mut iter, "--source")?),
            other => {
                return Err(TenancyError::Validation(format!(
                    "unknown flag `{other}` — audit-log takes --limit, --table, --pk, \
                     --operation, --source"
                )))
            }
        }
    }

    // The registry's log, matching the console page: tenant-side rows are
    // the tenant's own history and have their own viewer.
    let registry = pools.registry_pool();
    let total = crate::audit::count(&registry, &filter)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;
    let rows = crate::audit::list(&registry, &filter, limit, 0)
        .await
        .map_err(|e| TenancyError::Validation(e.to_string()))?;

    if rows.is_empty() {
        writeln!(w, "(no audit entries)")?;
        return Ok(());
    }
    writeln!(
        w,
        "{:<20} {:<22} {:<14} {:<12} source",
        "when", "table", "pk", "operation"
    )?;
    writeln!(w, "{}", "-".repeat(92))?;
    for e in &rows {
        writeln!(
            w,
            "{:<20} {:<22} {:<14} {:<12} {}",
            e.occurred_at.format("%Y-%m-%d %H:%M:%SZ"),
            clip(&e.entity_table, 22),
            clip(&e.entity_pk, 14),
            clip(&e.operation, 12),
            if e.source.is_empty() { "-" } else { &e.source },
        )?;
    }
    if total > limit {
        writeln!(w, "\nshowing {} of {total} — raise --limit", rows.len())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_marks_what_it_shortened() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("exactlyten", 10), "exactlyten");
        assert_eq!(clip("elevenchars", 10), "elevencha…");
        // Multi-byte input must not be sliced mid-character.
        assert_eq!(clip("ünïcödé-is-long", 6), "ünïcö…");
    }

    #[test]
    fn a_limit_must_be_a_positive_integer() {
        for bad in ["0", "-1", "lots"] {
            let args = vec![bad.to_owned()];
            let mut it = args.iter();
            assert!(
                parse_limit(&mut it, "--limit").is_err(),
                "`{bad}` should be refused"
            );
        }
        let args = vec!["50".to_owned()];
        let mut it = args.iter();
        assert_eq!(parse_limit(&mut it, "--limit").expect("ok"), 50);
    }
}
