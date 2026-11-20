//! `clear-cache`, `clearsessions` and `createcachetable`.

use super::*;

// =====================================================================
// `manage clear-cache` / `clearsessions` — DatabaseCache GC, which
// also clears sessions when the session backend is the DB cache.
// =====================================================================

#[cfg(feature = "cache")]
#[derive(Debug, Default, PartialEq)]
pub(super) struct ClearCacheArgs {
    /// `--table <name>` — cache table to GC. Defaults to
    /// `rustango_cache`, matching `DatabaseCache::default_table_name`.
    table: Option<String>,
    help: bool,
}

#[cfg(feature = "cache")]
pub(super) fn parse_clear_cache_args(args: &[String]) -> Result<ClearCacheArgs, MigrateError> {
    let mut out = ClearCacheArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                out.help = true;
                return Ok(out);
            }
            "--table" => {
                let v = iter.next().ok_or_else(|| missing_value("--table"))?;
                out.table = Some(v.clone());
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

/// `manage clear-cache [--table <name>]` (alias `clearsessions`) —
/// delete every expired row from a [`crate::cache::DatabaseCache`]
/// table. Pairs with the implicit lazy GC on `get` / `exists` to
/// reclaim space from keys nobody reads anymore.
///
/// Also clears sessions when they are backed by the DB cache.
///
/// Defaults the table to `rustango_cache`; pass `--table <name>` for
/// non-default DatabaseCache tables (`rustango_sessions`, app-
/// specific caches, etc.). Returns the number of rows deleted.
#[cfg(feature = "cache")]
pub(super) async fn clear_cache_cmd<W: Write>(
    pool: &Pool,
    verb: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_clear_cache_args(args)?;
    if parsed.help {
        writeln!(w, "clear-cache [--table <name>]")?;
        writeln!(w, "  (alias: clearsessions)")?;
        writeln!(w)?;
        writeln!(w, "  Delete every expired row from a DatabaseCache table.")?;
        writeln!(w, "  Default table: rustango_cache. Use --table for the")?;
        writeln!(w, "  session-backend table or any app-specific cache.")?;
        return Ok(());
    }
    let table = parsed.table.unwrap_or_else(|| "rustango_cache".to_owned());
    // No table, nothing cached: `migrate` does not create it. Resolved like
    // the purge's own query, through the search path.
    let exists = crate::migrate::runner::try_table_resolves(pool, &table)
        .await
        .map_err(|e| MigrateError::Validation(format!("{verb}: {e}")))?;
    if !exists {
        writeln!(
            w,
            "nothing to clear: no {table} table (run createcachetable if you use DatabaseCache)"
        )?;
        return Ok(());
    }
    let cache = crate::cache::DatabaseCache::new(pool.clone(), &table);
    let rows = cache
        .purge_expired()
        .await
        .map_err(|e| MigrateError::Validation(format!("{verb}: {e}")))?;
    writeln!(w, "{verb}: purged {rows} expired row(s) from {table}")?;
    Ok(())
}

/// `manage createcachetable [--table <name>]` (alias
/// `create-cache-table`) — idempotently create the
/// [`crate::cache::DatabaseCache`] table via the dialect's
/// `CREATE TABLE IF NOT EXISTS` DDL. Safe to call at every boot.
///
/// Defaults the table to `rustango_cache`. Pass `--table <name>` for
/// non-default DatabaseCache tables (one per app-specific cache, or
/// the session-backend table when sessions are routed to the DB).
#[cfg(feature = "cache")]
pub(super) async fn createcachetable_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    // Reuse the `clear-cache` arg parser — it accepts the same
    // `--table` knob with no other state, so no need to duplicate
    // the parsing logic.
    let parsed = parse_clear_cache_args(args)?;
    if parsed.help {
        writeln!(w, "createcachetable [--table <name>]")?;
        writeln!(w, "  (alias: create-cache-table)")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Idempotently CREATE the DatabaseCache table via per-dialect DDL."
        )?;
        writeln!(
            w,
            "  Default table: rustango_cache. Safe to call at every boot."
        )?;
        return Ok(());
    }
    let table = parsed.table.unwrap_or_else(|| "rustango_cache".to_owned());
    let cache = crate::cache::DatabaseCache::new(pool.clone(), &table);
    cache
        .ensure_table()
        .await
        .map_err(|e| MigrateError::Validation(format!("createcachetable: {e}")))?;
    writeln!(w, "createcachetable: ensured table {table} exists")?;
    Ok(())
}
