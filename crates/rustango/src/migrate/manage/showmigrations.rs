//! `sqlmigrate`, `showmigrations` and `forget-pending`.

use super::*;

/// `sqlmigrate <name>` — print the SQL that would run when the named
/// migration is applied, without touching the database.
///
/// Output format mirrors `migrate --dry-run` per-migration: a comment
/// header (`-- <name> (atomic|non-atomic)`) followed by every emitted
/// statement, semicolon-terminated, one per line.
pub(super) fn sqlmigrate_cmd<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut positional: Option<&str> = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {
                writeln!(
                    w,
                    "sqlmigrate <name>          print the SQL the named migration would emit\n\
                                                without applying it"
                )?;
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                if positional.is_some() {
                    return Err(unexpected_positional(other));
                }
                positional = Some(other);
            }
        }
    }
    let name = positional
        .ok_or_else(|| MigrateError::Validation("sqlmigrate requires a migration name".into()))?;
    // A framework migration lives in the system chain's dir.
    let system = crate::migrate::make::SystemChain::dir_for(dir);
    let in_system = !file::list_dir(dir)?.iter().any(|m| m.name == name)
        && system.is_dir()
        && file::list_dir(&system)?.iter().any(|m| m.name == name);
    let src = if in_system { system.as_path() } else { dir };
    let preview = runner::sqlmigrate_one(src, name, pool.dialect())?;
    writeln!(
        w,
        "-- {} ({})",
        preview.name,
        if preview.atomic {
            "atomic"
        } else {
            "non-atomic"
        }
    )?;
    for stmt in &preview.statements {
        writeln!(w, "{stmt};")?;
    }
    Ok(())
}

pub(super) async fn showmigrations<W: Write>(
    pool: &Pool,
    dir: &Path,
    w: &mut W,
) -> Result<(), MigrateError> {
    showmigrations_with(pool, dir, &SystemView::tenant(dir), None, w).await
}

/// Each tenant's slug and the names one of its ledgers records.
pub(crate) type TenantLedgers = [(String, std::collections::HashSet<String>)];

/// Every tenant's project and system ledgers.
pub(crate) struct TenantChains<'a> {
    pub(crate) project: &'a TenantLedgers,
    pub(crate) system: &'a TenantLedgers,
}

/// `showmigrations` as `migrate` reads the ledgers: squashed files count as
/// applied, and `system`'s chain is listed too. With `tenants`,
/// tenant-scoped files of both chains are marked from their ledgers.
///
/// # Errors
/// File I/O or a ledger read.
pub(crate) async fn showmigrations_with<W: Write>(
    pool: &Pool,
    dir: &Path,
    system: &SystemView,
    tenants: Option<&TenantChains<'_>>,
    w: &mut W,
) -> Result<(), MigrateError> {
    use crate::migrate::MigrationScope;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_or_empty(pool, runner::LEDGER_TABLE).await?;
    if all.is_empty() {
        writeln!(w, "(no migrations in {})", dir.display())?;
    } else {
        writeln!(w, "Migrations in {}:", dir.display())?;
    }
    chain_lines(w, &all, &applied, tenants.map(|t| t.project), |_| true)?;

    let Some(files) = system.files()? else {
        writeln!(w, "System migrations: {NOT_GENERATED}")?;
        return Ok(());
    };
    let applied = runner::applied_or_empty(pool, runner::SYSTEM_LEDGER_TABLE).await?;
    writeln!(w, "System migrations in {}:", system.dir.display())?;
    // A tenancy registry lists the tenant steps from the tenants' ledgers.
    let tenant_steps = tenants.is_some();
    chain_lines(w, &files, &applied, tenants.map(|t| t.system), |m| {
        m.scope == system.scope || (tenant_steps && m.scope == MigrationScope::Tenant)
    })
}

/// One chain's `shown` files: tenant-scoped ones from `tenants` when given,
/// the rest from `applied`.
pub(super) fn chain_lines<W: Write>(
    w: &mut W,
    all: &[Migration],
    applied: &std::collections::HashSet<String>,
    tenants: Option<&TenantLedgers>,
    shown: impl Fn(&Migration) -> bool,
) -> Result<(), MigrateError> {
    use runner::MigrationState as S;
    let per_tenant: Vec<Vec<S>> = tenants
        .unwrap_or_default()
        .iter()
        .map(|(_, done)| {
            runner::migration_states(all, done)
                .map(|(_, s)| s)
                .collect()
        })
        .collect();
    for (i, (m, state)) in runner::migration_states(all, applied).enumerate() {
        if !shown(m) {
            continue;
        }
        if tenants.is_some() && m.scope == crate::migrate::MigrationScope::Tenant {
            let n = per_tenant.len();
            if n == 0 {
                // Nothing is owed with no tenant to apply it to.
                writeln!(w, "  [-] {} (no tenants)", m.name)?;
                continue;
            }
            let done = per_tenant.iter().filter(|t| t[i] != S::Pending).count();
            let mark = match done {
                0 => "[ ]",
                d if d == n => "[X]",
                _ => "[~]",
            };
            writeln!(w, "  {mark} {} ({done}/{n} tenants)", m.name)?;
        } else {
            writeln!(w, "  {}", state_line(m, state))?;
        }
    }
    Ok(())
}

pub(super) fn state_line(m: &Migration, state: runner::MigrationState) -> String {
    match state {
        runner::MigrationState::Applied => format!("[X] {}", m.name),
        runner::MigrationState::Replaced => format!("[X] {} (replaced by a squash)", m.name),
        runner::MigrationState::Pending => format!("[ ] {}", m.name),
    }
}

/// `manage forget-pending <name>` — delete a migration JSON that
/// hasn't been applied yet, so the next `makemigrations` regenerates
/// the diff from the current model registry. The actionable
/// companion to the dev-iteration recovery path documented in the
/// `AddColumn NOT NULL no default` validator error (#84).
///
/// Refuses if:
///   - the migration name doesn't match any file in `dir`
///   - the migration is recorded in `__rustango_migrations__` (i.e.
///     it's been applied — removing the JSON without unapplying
///     would orphan the ledger row and break further migrate runs)
///   - more than one migration matches a partial-name query (use
///     the full name to disambiguate)
///
/// On success: deletes the file, prints a one-line confirmation,
/// and suggests `makemigrations` as the follow-up.
pub(super) async fn forget_pending_cmd<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    forget_pending_cmd_with(pool, dir, args, w, async { Ok(Vec::new()) }).await
}

/// [`forget_pending_cmd`], also refusing a migration any of `tenant_ledgers`
/// (slug, applied names) records. Awaited only once a name is resolved (#2393).
pub(crate) async fn forget_pending_cmd_with<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
    tenant_ledgers: impl std::future::Future<
        Output = Result<Vec<(String, std::collections::HashSet<String>)>, MigrateError>,
    >,
) -> Result<(), MigrateError> {
    // --help / no args
    let target = match args.first().map(String::as_str) {
        Some("--help") | Some("-h") | None => {
            writeln!(w, "forget-pending <name>")?;
            writeln!(
                w,
                "  Delete a migration JSON that has NOT been applied yet, so the"
            )?;
            writeln!(
                w,
                "  next `makemigrations` regenerates the diff against current"
            )?;
            writeln!(
                w,
                "  models. Refuses to delete an already-applied migration (would"
            )?;
            writeln!(
                w,
                "  orphan the `{}` ledger row and break later runs).",
                runner::LEDGER_TABLE
            )?;
            writeln!(w)?;
            writeln!(
                w,
                "  <name> can be the full migration name (e.g. `0003_auto`)"
            )?;
            writeln!(w, "  or a unique substring; ambiguous matches error.")?;
            return Ok(());
        }
        Some(s) if s.starts_with('-') => {
            return Err(MigrateError::Validation(format!(
                "forget-pending: expected positional <name> first, got flag `{s}`"
            )));
        }
        Some(s) => s,
    };

    runner::ensure_ledger_pool(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_set_pool(pool).await?;

    // Resolve <name> against the file list — accept exact match OR
    // unique substring (matches the "0003" use case).
    let exact: Vec<&Migration> = all.iter().filter(|m| m.name == target).collect();
    let candidates: Vec<&Migration> = if exact.len() == 1 {
        exact
    } else {
        all.iter().filter(|m| m.name.contains(target)).collect()
    };
    let migration = match candidates.len() {
        0 => {
            return Err(MigrateError::Validation(format!(
                "forget-pending: no migration matches `{target}` in {}",
                dir.display()
            )));
        }
        1 => candidates[0],
        n => {
            let names: Vec<&str> = candidates.iter().map(|m| m.name.as_str()).collect();
            return Err(MigrateError::Validation(format!(
                "forget-pending: `{target}` is ambiguous ({n} matches: {}); pass the full migration name",
                names.join(", ")
            )));
        }
    };

    // Refuse if the migration has been applied — removing it would
    // orphan the ledger row.
    if applied.contains(&migration.name) {
        return Err(MigrateError::Validation(format!(
            "forget-pending: migration `{}` is already applied (recorded in `{}`). \
             Use `migrate <prev>` or `downgrade` to unapply it first, then \
             `forget-pending` to drop the JSON.",
            migration.name,
            runner::LEDGER_TABLE,
        )));
    }
    if let Some((slug, _)) = tenant_ledgers
        .await?
        .into_iter()
        .find(|(_, applied)| applied.contains(&migration.name))
    {
        return Err(MigrateError::Validation(format!(
            "forget-pending: migration `{}` is already applied on tenant `{slug}`. \
             Unapply it there first.",
            migration.name,
        )));
    }

    // Only a leaf: a later file naming it in `prev`/`merges` breaks the chain.
    let dependants: Vec<&str> = all
        .iter()
        .filter(|m| {
            m.prev.as_deref() == Some(migration.name.as_str()) || m.merges.contains(&migration.name)
        })
        .map(|m| m.name.as_str())
        .collect();
    if !dependants.is_empty() {
        return Err(MigrateError::Validation(format!(
            "forget-pending: `{}` is built on by {}; forget those first",
            migration.name,
            dependants.join(", "),
        )));
    }

    let path = file_path(dir, &migration.name);
    std::fs::remove_file(&path).map_err(|e| {
        MigrateError::Validation(format!("forget-pending: rm {}: {e}", path.display()))
    })?;
    writeln!(w, "deleted {}", path.display())?;
    writeln!(w, "  next: `cargo run -- makemigrations` will regenerate")?;
    writeln!(w, "  the diff against the current model registry.")?;
    Ok(())
}
