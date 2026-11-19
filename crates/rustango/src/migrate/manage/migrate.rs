//! `migrate`, its dry run and squash, and `downgrade`.

use super::*;

pub(super) async fn migrate<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut dry_run = false;
    let mut squash = false;
    let mut positional: Option<&str> = None;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--squash" => squash = true,
            "--help" | "-h" => {
                writeln!(
                    w,
                    "migrate                    apply pending migrations\n\
                     migrate <target>           forward or back to <target> (`zero` wipes)\n\
                     migrate --dry-run          preview the SQL without writing\n\
                     migrate --squash           delete every pending (un-applied) migration JSON\n\
                                                and regenerate a single fresh diff. Dev-iteration\n\
                                                escape hatch — refuses to touch applied rows."
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

    if squash {
        if dry_run || positional.is_some() {
            return Err(MigrateError::Validation(
                "`migrate --squash` does not combine with `--dry-run` or a positional target"
                    .into(),
            ));
        }
        return migrate_squash(pool, dir, w).await;
    }

    if dry_run {
        if positional.is_some() {
            return Err(MigrateError::Validation(
                "`migrate <target> --dry-run` is not supported — use plain `--dry-run` to preview pending forward migrations".into(),
            ));
        }
        let dirs = [dir.to_path_buf()];
        return migrate_dry_run_cmd(pool, &dirs, &SystemView::tenant(dir), w).await;
    }

    if let Some(target) = positional {
        if target != "zero" {
            // A typo must fail before the system chain touches the database.
            runner::target_on_disk(&crate::migrate::file::list_dir(dir)?, dir, target)?;
        }
        // A forward target needs the framework tables its migrations may FK.
        if target != "zero" && !target_is_applied(pool, target).await? {
            // Files after the target are rolled back first: only the rest
            // count as applied, in the output and in `post_migrate`.
            let chain = system_chain(dir)?;
            let moved = crate::migrate::chains::migrate_chains_to(
                pool,
                &chain,
                crate::migrate::MigrationScope::Tenant,
                &[dir.to_path_buf()],
                runner::Signals::Fire { tenant: None },
                dir,
                target,
            )
            .await?;
            // `None`: applied meanwhile, so the move is backward.
            if let Some(applied) = moved {
                for m in &applied.unapplied {
                    writeln!(w, "  unapplied {}", m.name)?;
                }
                return after_chains(pool, &applied, w).await;
            }
        }
        let touched = runner::migrate_to_pool(pool, dir, target).await?;
        if touched.is_empty() {
            writeln!(w, "already at {target}")?;
        } else {
            // One list holds both directions: the ledger says which each went.
            let applied = runner::applied_set_pool(pool).await?;
            for m in &touched {
                let verb = if applied.contains(&m.name) {
                    "applied"
                } else {
                    "unapplied"
                };
                writeln!(w, "  {verb} {}", m.name)?;
            }
        }
        return Ok(());
    }

    migrate_with_framework(
        pool,
        dir,
        w,
        runner::Signals::Fire { tenant: None },
        |held| runner::migrate_pool_locked(held, pool, dir, None),
    )
    .await
}

pub(super) async fn target_is_applied(pool: &Pool, target: &str) -> Result<bool, MigrateError> {
    let applied = runner::with_migrate_lock_held(pool, |held| {
        runner::ledger_names(held, pool, runner::LEDGER_TABLE)
    })
    .await?;
    Ok(applied.contains(target))
}

/// The framework chain `migrate` runs beside the project's, read from disk
/// as is: a preview never generates it.
pub(crate) struct SystemView {
    pub(super) dir: std::path::PathBuf,
    pub(super) scope: crate::migrate::MigrationScope,
    /// The scopes `migrate` generates steps for before it runs.
    generates: &'static [crate::core::ModelScope],
}

impl SystemView {
    /// What a single-database `migrate` runs: the tenant-scope steps.
    pub(crate) fn tenant(dir: &Path) -> Self {
        Self {
            dir: crate::migrate::make::SystemChain::dir_for(dir),
            scope: crate::migrate::MigrationScope::Tenant,
            generates: &[
                crate::core::ModelScope::Registry,
                crate::core::ModelScope::Tenant,
            ],
        }
    }

    /// What a tenancy registry runs.
    pub(crate) fn registry(dir: &Path) -> Self {
        Self {
            dir: crate::migrate::make::SystemChain::dir_for(dir),
            scope: crate::migrate::MigrationScope::Registry,
            generates: &[crate::core::ModelScope::Registry],
        }
    }

    /// The chain as `migrate` runs it: the files, then the steps it would
    /// generate from today's models, unwritten.
    fn planned(&self, files: Option<Vec<Migration>>) -> Result<Vec<Migration>, MigrateError> {
        let mut all = files.unwrap_or_default();
        for &scope in self.generates {
            if let Some(step) = crate::migrate::make::plan_system_step(scope, None, &all)? {
                all.push(step);
            }
        }
        Ok(all)
    }

    /// The chain's files, or `None` before `migrate` first generates them.
    pub(super) fn files(&self) -> Result<Option<Vec<Migration>>, MigrateError> {
        if !self.dir.is_dir() {
            return Ok(None);
        }
        let all = file::list_dir(&self.dir)?;
        Ok((!all.is_empty()).then_some(all))
    }
}

pub(super) const NOT_GENERATED: &str = "the framework's system migrations are not on disk yet; \
                             `migrate` generates and applies them";

/// `migrate --dry-run`: what `migrate` runs from `system`, then from each
/// of `dirs` in order.
///
/// # Errors
/// As [`runner::preview_pending`].
pub(crate) async fn migrate_dry_run_cmd<W: Write>(
    pool: &Pool,
    dirs: &[std::path::PathBuf],
    system: &SystemView,
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut previews = Vec::new();
    let system_files = system.files()?;
    let not_generated = system_files.is_none();
    let all = system.planned(system_files)?;
    let ledger = runner::SYSTEM_LEDGER_TABLE;
    let owned = crate::migrate::chains::project_owned(dirs)?;
    let steps = runner::preview_pending(
        pool,
        &system.dir,
        &all,
        Some(system.scope),
        ledger,
        true,
        &owned,
    )
    .await?;
    previews.extend(steps.into_iter().map(|p| (p, "system, ")));
    for dir in dirs {
        let project = runner::migrate_dry_run_pool(pool, dir).await?;
        previews.extend(project.into_iter().map(|p| (p, "")));
    }
    // What `migrate` runs after both chains.
    let mut after = Vec::new();
    let converge = crate::migrate::chains::converge_preview(pool, &all, system.scope, dirs).await?;
    if !converge.is_empty() {
        after.push(("converge framework tables".to_owned(), converge));
    }
    let tenant = system.scope == crate::migrate::MigrationScope::Tenant;
    let bootstrap = bootstrap_tables(BootstrapHolder::Shared).into_iter().chain(
        tenant
            .then(|| bootstrap_tables(BootstrapHolder::Tenant))
            .into_iter()
            .flatten(),
    );
    let created = crate::migrate::chains::pending_creates(pool, &all, system.scope, dirs).await?;
    for b in bootstrap {
        if created.contains(b.table) || runner::try_table_exists_here(pool, b.table).await? {
            continue;
        }
        let ddl = (b.ddl)(pool.dialect()).map_err(MigrateError::Validation)?;
        let ddl = ddl
            .into_iter()
            .map(crate::migrate::chains::PreviewLine::Sql)
            .collect();
        after.push((format!("bootstrap {}", b.table), ddl));
    }
    if not_generated {
        writeln!(w, "-- {NOT_GENERATED}")?;
    }
    if previews.is_empty() && after.is_empty() {
        writeln!(w, "nothing to migrate (already up to date)")?;
        return Ok(());
    }
    writeln!(
        w,
        "-- DRY RUN: {} pending migration(s); no SQL will be executed",
        previews.len()
    )?;
    for (p, chain) in &previews {
        writeln!(w)?;
        let atomic = if p.atomic { "atomic" } else { "non-atomic" };
        writeln!(w, "-- {} ({chain}{atomic})", p.name)?;
        for stmt in &p.statements {
            writeln!(w, "{stmt};")?;
        }
    }
    for (label, lines) in &after {
        writeln!(w)?;
        writeln!(w, "-- {label} (idempotent)")?;
        for line in lines {
            match line {
                crate::migrate::chains::PreviewLine::Sql(stmt) => writeln!(w, "{stmt};")?,
                crate::migrate::chains::PreviewLine::Note(note) => writeln!(w, "-- {note}")?,
            }
        }
    }
    Ok(())
}

/// Everything a plain `migrate` does: the framework's system chain and the
/// project chain via `project`, in [`crate::migrate::chains::migrate_chains`]'s order, then
/// the framework bootstrap DDL. The `Cli` auto-migrate shares it (#2056).
///
/// In tenancy mode the system chain is applied per tenant by
/// `tenancy::migrate`; a single-database project gets it here, before its
/// own migrations so user models may FK framework tables (#1171).
///
/// # Errors
/// As [`crate::migrate::chains::migrate_chains`], plus the bootstrap DDL.
pub(crate) async fn migrate_with_framework<W, F, Fut>(
    pool: &Pool,
    dir: &Path,
    w: &mut W,
    signals: crate::migrate::Signals<'_>,
    project: F,
) -> Result<(), MigrateError>
where
    W: Write,
    F: FnOnce(crate::migrate::LockHeld) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Migration>, MigrateError>>,
{
    let chain = system_chain(dir)?;
    let applied = crate::migrate::chains::migrate_chains(
        pool,
        &chain,
        crate::migrate::MigrationScope::Tenant,
        &[dir.to_path_buf()],
        None,
        signals,
        project,
    )
    .await?;
    after_chains(pool, &applied, w).await
}

/// What both chains applied, then the framework bootstrap DDL.
pub(super) async fn after_chains<W: Write>(
    pool: &Pool,
    applied: &crate::migrate::chains::Applied,
    w: &mut W,
) -> Result<(), MigrateError> {
    for m in &applied.system {
        writeln!(w, "  applied {} (system)", m.name)?;
    }
    for m in &applied.project {
        writeln!(w, "  applied {}", m.name)?;
    }
    if applied.system.is_empty() && applied.project.is_empty() {
        writeln!(w, "nothing to migrate (already up to date)")?;
    }
    ensure_bootstrap_tables(pool).await?;
    ensure_tenant_bootstrap_tables(pool).await?;

    // #1464 — rows written by the pre-fix SQLite default are stored as
    // `YYYY-MM-DD HH:MM:SS` and do not compare or sort against a
    // timestamp bound from Rust. Correcting the DDL fixes new rows and
    // leaves every existing database silently wrong, so the stored
    // values are converted here.
    //
    // Unconditional rather than behind a repair flag, for the reason
    // the bug itself demonstrates: an operator does not know to run a
    // verb they have not heard of, and silence reads exactly like "you
    // are fine". Runs after the migrations above because it needs the
    // tables to exist, and it is idempotent — the mask stops matching
    // once a value is converted — so a second `migrate` updates nothing.
    #[cfg(feature = "sqlite")]
    {
        let fixed = crate::migrate::sqlite_datetime::normalise_sqlite_datetimes(pool).await?;
        if !fixed.is_clean() {
            writeln!(
                w,
                "  normalised {} SQLite datetime value(s) in {} — these were written by \
                 the pre-0.57.11 default and did not compare against a bound timestamp",
                fixed.rows,
                fixed.columns.join(", "),
            )?;
        }
    }
    Ok(())
}

/// The `managed = false` framework tables, created by idempotent DDL on the
/// database that holds them: the single database, or a tenancy registry.
///
/// # Errors
/// Driver or SQL failures from the DDL.
pub(crate) async fn ensure_bootstrap_tables(pool: &Pool) -> Result<(), sqlx::Error> {
    for b in bootstrap_tables(BootstrapHolder::Shared) {
        (b.ensure)(pool).await?;
    }
    Ok(())
}

/// The `managed = false` tables each tenant holds: the single database, or
/// every tenant. Never a registry, where schema-mode tenants would share them.
///
/// # Errors
/// Driver or SQL failures from the DDL.
pub(crate) async fn ensure_tenant_bootstrap_tables(pool: &Pool) -> Result<(), sqlx::Error> {
    for b in bootstrap_tables(BootstrapHolder::Tenant) {
        (b.ensure)(pool).await?;
    }
    Ok(())
}

/// `migrate --squash` (#84a) — dev-iteration escape hatch.
///
/// Sets aside every pending (un-applied) migration JSON in `dir`, then
/// re-runs `makemigrations` so the diff regenerates as a single
/// fresh file against the current model registry. The pending files are
/// deleted once that succeeds and put back on any failure. Recovers the
/// "scaffolder shape → real shape" iteration cycle that hits the
/// `AddColumn NOT NULL no default` validator rejection without
/// touching the database.
///
/// Refuses if any pending migration is somehow already applied
/// (impossible by definition — "pending" means absent from the
/// ledger — but the check is cheap and guards against a future
/// caller passing a stale `applied_set`). Refuses if there are zero
/// pending migrations (nothing to do) or only one (`forget-pending`
/// is the right verb for the single-file case).
pub(super) async fn migrate_squash<W: Write>(
    pool: &Pool,
    dir: &Path,
    w: &mut W,
) -> Result<(), MigrateError> {
    SetAside::refuse_leftovers(dir)?;
    runner::ensure_ledger_pool(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_set_pool(pool).await?;

    let pending: Vec<&Migration> = all.iter().filter(|m| !applied.contains(&m.name)).collect();
    if pending.is_empty() {
        writeln!(
            w,
            "no pending migrations to squash (every JSON is in the ledger)"
        )?;
        return Ok(());
    }
    if pending.len() == 1 {
        return Err(MigrateError::Validation(format!(
            "only one pending migration (`{}`) — use `forget-pending {}` instead of `--squash`",
            pending[0].name, pending[0].name,
        )));
    }

    // Defensive double-check: the filter above already guarantees
    // none of these are applied. If a future refactor breaks that
    // invariant we still fail loudly instead of clobbering the
    // ledger.
    for m in &pending {
        if applied.contains(&m.name) {
            return Err(MigrateError::Validation(format!(
                "migrate --squash: refused — migration `{}` appears applied. \
                 Use `migrate <prev>` or `downgrade` to unapply it first.",
                m.name,
            )));
        }
    }

    // A re-diff loses these: data and callbacks vanish, a rename turns into drop + add.
    for m in &pending {
        if let Some((i, kind)) = m.forward.iter().enumerate().find_map(|(i, op)| {
            let kind = match op {
                Operation::Data(_) => "data op",
                Operation::Callback(_) => "callback",
                Operation::Schema(crate::migrate::SchemaChange::RenameTable { .. }) => {
                    "RenameTable"
                }
                Operation::Schema(crate::migrate::SchemaChange::RenameColumn { .. }) => {
                    "RenameColumn"
                }
                Operation::Schema(_) => return None,
            };
            Some((i, kind))
        }) {
            return Err(MigrateError::Validation(format!(
                "migrate --squash: refused — `{}` forward[{i}] is a {kind}, which a fresh diff \
                 would lose. Fold the pending files by hand instead.",
                m.name,
            )));
        }
    }

    // The squash is built from the models; on a database that ran the old
    // files it is only recorded, so a change no file holds would never run.
    if crate::migrate::make::models_ahead_of(&all, has_registry_models())? {
        return Err(MigrateError::Validation(
            "migrate --squash: refused — the models have changes no migration holds yet. \
             Run `makemigrations` first, then squash."
                .into(),
        ));
    }

    writeln!(
        w,
        "squashing {} pending migration(s) into a fresh diff:",
        pending.len()
    )?;
    // Remember what we collapse, per scope, so the regenerated file can
    // declare it `replaces` them (see below).
    let removed: Vec<(String, crate::migrate::MigrationScope)> =
        pending.iter().map(|m| (m.name.clone(), m.scope)).collect();
    // Set aside, not deleted: a failed regeneration (or a panic) puts them back.
    let aside = SetAside::take(dir, pending.iter().map(|m| m.name.as_str()))?;
    let regenerated = (|| {
        for (path, _) in &aside.moved {
            writeln!(w, "  removed {}", path.display())?;
        }
        writeln!(w, "regenerating diff against current model registry...")?;
        // Empty args means "default makemigrations behavior" — splits
        // registry vs tenant in tenancy projects, single file otherwise.
        // Pass through to the existing entry point so any future flags
        // gain consistent behavior automatically.
        // The pending files already hold any drop + add; refusing here would lose them.
        makemigrations(dir, &["--drop-and-add".to_owned()], w)?;
        stamp_replaces(dir, &aside.held_before(), &removed, w)
    })();
    match regenerated {
        Ok(()) => {
            for e in aside.discard() {
                writeln!(w, "  {e}")?;
            }
            Ok(())
        }
        Err(e) => match aside.restore() {
            Ok(()) => Err(e),
            Err(r) => Err(MigrateError::Validation(format!(
                "{e}; restoring the pending files failed too: {r}"
            ))),
        },
    }
}

/// Suffix of a pending file `migrate --squash` has set aside.
pub(super) const SQUASH_BACKUP: &str = "json.squash-bak";

/// The pending files `migrate --squash` renamed to `*.json.squash-bak`, and
/// the folders it may write. Dropped unspent (a panic), it restores them.
pub(super) struct SetAside {
    /// `(file, backup)` pairs, in the order they were moved.
    moved: Vec<(std::path::PathBuf, std::path::PathBuf)>,
    /// Each folder the run writes, with the `*.json` names it held before
    /// (pending ones excluded); `None` when the folder did not exist.
    watched: Vec<(
        std::path::PathBuf,
        Option<std::collections::HashSet<String>>,
    )>,
    spent: bool,
}

impl SetAside {
    /// Refuse a `dir` holding backups a crashed squash left behind.
    fn refuse_leftovers(dir: &Path) -> Result<(), MigrateError> {
        let mut left: Vec<String> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                .filter(|n| n.ends_with(&format!(".{SQUASH_BACKUP}")))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if left.is_empty() {
            return Ok(());
        }
        left.sort();
        Err(MigrateError::Validation(format!(
            "migrate --squash: refused — {} holds pending files an interrupted squash set \
             aside: {}. Rename each back to `.json` (drop `.squash-bak`) to keep it, or \
             delete it, then run again.",
            dir.display(),
            left.join(", "),
        )))
    }

    /// Note the folders' files, then rename each of `names` aside; a
    /// failure puts back the ones moved.
    fn take<'a>(dir: &Path, names: impl Iterator<Item = &'a str>) -> Result<Self, MigrateError> {
        let names: Vec<&str> = names.collect();
        // Tenancy projects also write the framework chain next to `dir`;
        // `dir` comes first (see `held_before`).
        let system = dir
            .parent()
            .unwrap_or(dir)
            .join("system")
            .join("migrations");
        let mut watched = Vec::new();
        for folder in [dir.to_path_buf(), system] {
            let held = if folder.exists() {
                let mut held = json_names(&folder)?;
                if folder == dir {
                    for n in &names {
                        held.remove(*n);
                    }
                }
                Some(held)
            } else {
                None
            };
            watched.push((folder, held));
        }
        let mut aside = Self {
            moved: Vec::new(),
            watched,
            spent: false,
        };
        for (i, name) in names.iter().enumerate() {
            let path = file_path(dir, name);
            let backup = path.with_extension(SQUASH_BACKUP);
            if let Err(e) = std::fs::rename(&path, &backup) {
                // Still in place: not the run's to delete when `aside` drops.
                if let Some(held) = aside.watched[0].1.as_mut() {
                    held.extend(names[i..].iter().map(|n| (*n).to_owned()));
                }
                return Err(MigrateError::Validation(format!(
                    "migrate --squash: move {}: {e}",
                    path.display()
                )));
            }
            aside.moved.push((path, backup));
        }
        Ok(aside)
    }

    /// The `*.json` names the migrations folder held before the run,
    /// pending ones excluded.
    fn held_before(&self) -> std::collections::HashSet<String> {
        self.watched[0].1.clone().unwrap_or_default()
    }

    /// Success: delete the backups. Returns what could not be deleted.
    fn discard(mut self) -> Vec<String> {
        self.spent = true;
        self.moved
            .iter()
            .filter_map(|(_, backup)| {
                let e = std::fs::remove_file(backup).err()?;
                Some(format!("could not delete {}: {e}", backup.display()))
            })
            .collect()
    }

    /// Failure: delete what the run wrote, then put the originals back.
    fn restore(mut self) -> Result<(), MigrateError> {
        self.spent = true;
        let errors = self.undo();
        if errors.is_empty() {
            return Ok(());
        }
        Err(MigrateError::Validation(format!(
            "migrate --squash: {}",
            errors.join("; ")
        )))
    }

    /// Try every delete and rename, in that order; returns every failure.
    fn undo(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for (folder, held) in &self.watched {
            let now = match json_names(folder) {
                Ok(now) => now,
                Err(MigrateError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    errors.push(format!("list {}: {e}", folder.display()));
                    continue;
                }
            };
            let new = now
                .iter()
                .filter(|n| held.as_ref().is_none_or(|h| !h.contains(*n)));
            for name in new {
                let path = file_path(folder, name);
                if let Err(e) = std::fs::remove_file(&path) {
                    errors.push(format!("delete {}: {e}", path.display()));
                }
            }
            if held.is_none() {
                // Ours alone; a folder holding anything else stays.
                let _ = std::fs::remove_dir(folder);
            }
        }
        for (path, backup) in &self.moved {
            if let Err(e) = std::fs::rename(backup, path) {
                errors.push(format!(
                    "restore {} from {}: {e}",
                    path.display(),
                    backup.display()
                ));
            }
        }
        errors
    }
}

impl Drop for SetAside {
    fn drop(&mut self) {
        if self.spent {
            return;
        }
        for e in self.undo() {
            tracing::error!(target: "rustango::migrate", "migrate --squash: {e}");
        }
    }
}

/// The names of the `*.json` migration files in `dir`.
pub(super) fn json_names(dir: &Path) -> Result<std::collections::HashSet<String>, MigrateError> {
    Ok(std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .filter_map(|p| Some(p.file_stem()?.to_str()?.to_owned()))
        .collect())
}

/// The system chain a single-database project applies.
///
/// Only the **tenant**-scope chain runs. The two scopes exist because a
/// tenancy deployment splits them across different databases, and they
/// deliberately overlap on the shared framework tables (`rustango_audit_log`,
/// `rustango_content_types`) — applying both into one database would have the
/// second chain collide with the first. Tenant scope is also the right set on
/// its own: the registry-only tables (`rustango_orgs`, `rustango_operators`)
/// describe a tenant fleet and mean nothing to a single-database app, while
/// everything an app actually uses (users, roles, permissions, api keys,
/// audit log, content types, media, …) is tenant-scoped.
///
/// Generation is a no-op once the files exist (normally written by
/// `makemigrations` and committed); a generation error fails `migrate` (#2014).
pub(super) fn system_chain(dir: &Path) -> Result<crate::migrate::make::SystemChain, MigrateError> {
    crate::migrate::make::SystemChain::for_migrations_dir(
        dir,
        &[
            crate::core::ModelScope::Registry,
            crate::core::ModelScope::Tenant,
        ],
    )
}

/// Record, on each freshly-generated migration, the names it collapsed.
///
/// The squashed files were only *pending here* — another deployment (a
/// colleague's database, staging, CI) may well have applied some of them
/// already. Declaring `replaces` means the regenerated file **reconciles**
/// there (recorded + predecessors tombstoned, no DDL) instead of colliding
/// with tables those migrations created.
///
/// Names are matched to the new file by [`crate::migrate::MigrationScope`], so in a
/// tenancy project the registry squash claims only registry-scoped
/// predecessors and the tenant squash only tenant-scoped ones.
pub(super) fn stamp_replaces<W: Write>(
    dir: &Path,
    before: &std::collections::HashSet<String>,
    removed: &[(String, crate::migrate::MigrationScope)],
    w: &mut W,
) -> Result<(), MigrateError> {
    for mut mig in file::list_dir(dir)? {
        if before.contains(&mig.name) {
            continue; // pre-existing file, not one we just generated
        }
        let claims: Vec<String> = removed
            .iter()
            .filter(|(_, scope)| *scope == mig.scope)
            .map(|(name, _)| name.clone())
            .collect();
        if claims.is_empty() {
            continue;
        }
        writeln!(w, "  {} now replaces: {}", mig.name, claims.join(", "))?;
        mig.replaces = claims;
        file::write(&file_path(dir, &mig.name), &mig)?;
    }
    Ok(())
}

pub(super) async fn downgrade<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let steps: usize = if let Some(arg) = args.first() {
        arg.parse().map_err(|_| {
            MigrateError::Validation(format!(
                "invalid step count: {arg} (expected a non-negative integer)"
            ))
        })?
    } else {
        1
    };
    let touched = runner::downgrade_pool(pool, dir, steps).await?;
    if touched.is_empty() {
        writeln!(w, "nothing to downgrade")?;
    } else {
        for m in &touched {
            writeln!(w, "  rolled back {}", m.name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod squash_aside_tests {
    use super::*;

    /// `<root>/migrations` holding `0001_a` (applied) and the pending `0002_b`, `0003_c`.
    fn project() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["0001_a", "0002_b", "0003_c"] {
            std::fs::write(file_path(&dir, n), n).unwrap();
        }
        (root, dir)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    const PENDING: [&str; 2] = ["0002_b", "0003_c"];
    const ORIGINAL: [&str; 3] = ["0001_a.json", "0002_b.json", "0003_c.json"];

    /// A panic mid-squash puts the pending files back and drops what it wrote.
    #[test]
    fn a_panic_restores_the_pending_files() {
        let (root, dir) = project();
        let sys = root.path().join("system").join("migrations");
        let caught = std::panic::catch_unwind(|| {
            let _aside = SetAside::take(&dir, PENDING.into_iter()).unwrap();
            std::fs::write(file_path(&dir, "0002_new"), "new").unwrap();
            std::fs::create_dir_all(&sys).unwrap();
            std::fs::write(file_path(&sys, "0001_sys"), "sys").unwrap();
            panic!("mid-squash");
        });
        assert!(caught.is_err());
        assert_eq!(names(&dir), ORIGINAL);
        assert!(!sys.exists(), "the system folder it created is gone");
    }

    /// A folder that existed keeps its own files; only the new ones go.
    #[test]
    fn restore_removes_only_new_system_files() {
        let (root, dir) = project();
        let sys = root.path().join("system").join("migrations");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(file_path(&sys, "0001_sys"), "sys").unwrap();
        let aside = SetAside::take(&dir, PENDING.into_iter()).unwrap();
        std::fs::write(file_path(&sys, "0002_sys"), "sys").unwrap();
        aside.restore().unwrap();
        assert_eq!(names(&sys), ["0001_sys.json"]);
        assert_eq!(names(&dir), ORIGINAL);
    }

    /// One failed rename does not stop the others; every failure is reported.
    #[test]
    fn restore_tries_every_file() {
        let (_root, dir) = project();
        let aside = SetAside::take(&dir, PENDING.into_iter()).unwrap();
        std::fs::remove_file(dir.join(format!("0002_b.{SQUASH_BACKUP}"))).unwrap();
        let err = aside.restore().expect_err("0002_b's backup is gone");
        assert!(err.to_string().contains("0002_b"), "{err}");
        assert_eq!(names(&dir), ["0001_a.json", "0003_c.json"]);
    }

    /// A backup left by an interrupted squash refuses the next one.
    #[test]
    fn leftover_backups_refuse_a_squash() {
        let (_root, dir) = project();
        SetAside::refuse_leftovers(&dir).expect("no backups");
        std::fs::rename(
            file_path(&dir, "0003_c"),
            dir.join(format!("0003_c.{SQUASH_BACKUP}")),
        )
        .unwrap();
        let err = SetAside::refuse_leftovers(&dir).expect_err("a leftover");
        assert!(err.to_string().contains("0003_c.json.squash-bak"), "{err}");
    }
}
