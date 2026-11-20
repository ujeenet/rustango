//! `prune` and `audit-cleanup`.

use super::*;

// =====================================================================
// `manage prune` — bulk removal of stale rows from `Prunable` models.
// =====================================================================

#[derive(Debug, Default, PartialEq)]
pub(super) struct PruneArgs {
    /// `--model <table>` (repeatable) — restrict to listed tables.
    only: Vec<String>,
    /// `--except <table>` (repeatable) — skip listed tables.
    except: Vec<String>,
    /// `--pretend` — count matching rows but skip the actual DELETE.
    pretend: bool,
    help: bool,
}

pub(super) fn parse_prune_args(args: &[String]) -> Result<PruneArgs, MigrateError> {
    let mut out = PruneArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                out.help = true;
                return Ok(out);
            }
            "--pretend" | "--dry-run" => out.pretend = true,
            "--model" => {
                let v = iter.next().ok_or_else(|| missing_value("--model"))?;
                out.only.push(v.clone());
            }
            "--except" => {
                let v = iter.next().ok_or_else(|| missing_value("--except"))?;
                out.except.push(v.clone());
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                return Err(unexpected_positional(other));
            }
        }
    }
    Ok(out)
}

/// `manage audit-cleanup [--days <N> | --keep-last <N>] [--yes]`: audit-log
/// retention on the one database, the rule the tenancy verb uses. With
/// neither flag it deletes entries older than `[audit] retention_days`.
pub(super) async fn audit_cleanup_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    const USAGE: &str = "audit-cleanup [--days <N> | --keep-last <N>] [--yes]";
    let (mut days, mut keep_last, mut yes) = (None, None, false);
    let mut iter = args.iter();
    let number = |flag: &str, v: Option<&String>| -> Result<i64, MigrateError> {
        let raw = v.ok_or_else(|| missing_value(flag))?;
        raw.parse::<i64>().map_err(|_| {
            MigrateError::Validation(format!("{flag} expects an integer, got `{raw}`"))
        })
    };
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--days" => days = Some(number("--days", iter.next())?),
            "--keep-last" => keep_last = Some(number("--keep-last", iter.next())?),
            "--yes" => yes = true,
            "--help" | "-h" => {
                writeln!(w, "{USAGE}")?;
                return Ok(());
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "audit-cleanup: unexpected argument `{other}` — usage: {USAGE}"
                )))
            }
        }
    }
    let configured = if days.is_none() && keep_last.is_none() {
        crate::audit::Retention::configured_days().map_err(MigrateError::Validation)?
    } else {
        None
    };
    let rule = crate::audit::Retention::from_flags(days, keep_last, yes, configured)
        .map_err(MigrateError::Validation)?;
    let deleted = rule.prune(pool).await?;
    writeln!(w, "audit-cleanup done: deleted={deleted}")?;
    Ok(())
}

/// `manage prune [--model <name>] [--except <name>] [--pretend]` —
/// bulk-delete rows from every model implementing
/// [`crate::prunable::Prunable`] + registered via
/// `register_prunable!`. Issue #822 (Eloquent `Prunable` /
/// `model:prune` parity).
///
/// `--pretend` counts matching rows without deleting — useful for
/// previewing the impact against production data. `--model` /
/// `--except` are repeatable; `--except` wins on collision.
///
/// Each model's prune queryset is consulted in inventory order. The
/// FIRST failure short-circuits — successfully-pruned models stay
/// pruned (commits are NOT wrapped in an outer transaction; that
/// would defeat the streaming nature of bulk DELETE).
pub(super) async fn prune_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_prune_args(args)?;
    if parsed.help {
        writeln!(w, "prune [--model <name>] [--except <name>] [--pretend]")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Bulk-delete stale rows from every model implementing the"
        )?;
        writeln!(
            w,
            "  Prunable trait and registered via `register_prunable!`."
        )?;
        writeln!(w)?;
        writeln!(w, "  --model <name>   Restrict to this table (repeatable).")?;
        writeln!(
            w,
            "  --except <name>  Skip this table (repeatable). Beats --model."
        )?;
        writeln!(
            w,
            "  --pretend        Count matching rows; skip the DELETE."
        )?;
        let names = crate::prunable::registered_names();
        if names.is_empty() {
            writeln!(w)?;
            writeln!(w, "  (no prunable models currently registered)")?;
        } else {
            writeln!(w)?;
            writeln!(w, "  Registered: {}", names.join(", "))?;
        }
        return Ok(());
    }
    let opts = crate::prunable::PruneOptions {
        only: parsed.only,
        except: parsed.except,
    };
    if parsed.pretend {
        let reports = crate::prunable::prune_pretend(pool, &opts)
            .await
            .map_err(|e| MigrateError::Validation(format!("prune --pretend: {e}")))?;
        if reports.is_empty() {
            writeln!(w, "prune --pretend: no matching prunable models")?;
        } else {
            writeln!(w, "prune --pretend: would delete:")?;
            for r in &reports {
                writeln!(w, "  - {}: {} row(s)", r.table, r.rows)?;
            }
        }
    } else {
        let reports = crate::prunable::prune_all(pool, &opts)
            .await
            .map_err(|e| MigrateError::Validation(format!("prune: {e}")))?;
        if reports.is_empty() {
            writeln!(w, "prune: no matching prunable models")?;
        } else {
            let total: u64 = reports.iter().map(|r| r.rows).sum();
            writeln!(w, "prune: deleted {total} row(s):")?;
            for r in &reports {
                writeln!(w, "  - {}: {} row(s)", r.table, r.rows)?;
            }
        }
    }
    Ok(())
}
