//! `makemigrations`, empty and data migrations, `add-data-op`.

use super::*;

pub(super) fn makemigrations<W: Write>(
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut empty = false;
    let mut merge = false;
    let mut drops = crate::migrate::make::DropAndAdd::Refuse;
    let mut name: Option<String> = None;
    let mut app: Option<String> = None;
    let mut scope_override: Option<crate::core::ModelScope> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--empty" => empty = true,
            "--merge" => merge = true,
            "--drop-and-add" => drops = crate::migrate::make::DropAndAdd::Allow,
            "--app" => {
                app = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--app requires an app name".into())
                })?);
            }
            "--scope" => {
                let raw = iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--scope requires \"registry\" or \"tenant\"".into())
                })?;
                scope_override =
                    Some(crate::core::ModelScope::from_str(&raw).ok_or_else(|| {
                        MigrateError::Validation(format!(
                            "--scope must be \"registry\" or \"tenant\", got {raw:?}"
                        ))
                    })?);
            }
            "--help" | "-h" => {
                writeln!(
                    w,
                    "makemigrations [name]                  diff registry, write next file in <dir>\n\
                     makemigrations --app <app> [name]      diff one app, write to <project_root>/<app>/migrations/\n\
                     makemigrations --scope <s> [name]      <s> = registry|tenant; one file with that MigrationScope\n\
                     makemigrations --empty <name>          empty scaffold for data ops (--scope picks the chain)\n\
                     makemigrations --merge                 reconcile a divergent chain (two leaves\n\
                                                            on the same parent) by writing an empty\n\
                                                            NNNN_merge.json\n\
                     makemigrations --drop-and-add          write a dropped column beside a new one of\n\
                                                            the same type, or a dropped table beside a new\n\
                                                            one with its columns, as drop + add (deletes\n\
                                                            its data); refused by default as a likely rename\n\
                     \n\
                     In tenancy projects (any registered model with scope = \"registry\"),\n\
                     a flagless makemigrations splits the diff into TWO files — one for\n\
                     registry-scoped models, one for tenant-scoped — so framework tables\n\
                     don't bleed across scopes when migrate-tenants fans out."
                )?;
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                if name.is_some() {
                    return Err(unexpected_positional(other));
                }
                name = Some(other.to_owned());
            }
        }
    }

    if merge {
        if empty || name.is_some() || app.is_some() || scope_override.is_some() {
            return Err(MigrateError::Validation(
                "makemigrations --merge does not combine with --empty / --app / --scope / [name]"
                    .into(),
            ));
        }
        return run_merge(dir, w);
    }

    if empty {
        let Some(n) = name else {
            return Err(MigrateError::Validation(
                "makemigrations --empty requires a name".into(),
            ));
        };
        let scope = match scope_override {
            Some(crate::core::ModelScope::Registry) => crate::migrate::MigrationScope::Registry,
            Some(crate::core::ModelScope::Tenant) | None => crate::migrate::MigrationScope::Tenant,
        };
        let mig = make_empty_in_scope(dir, &n, scope)?;
        writeln!(
            w,
            "wrote {} (empty scaffold — fill in `forward` with data ops)",
            file_path(dir, &mig.name).display()
        )?;
        return Ok(());
    }

    // Per-app makemigrations (slice 9.0g): filter inventory to models
    // whose `resolved_app_label()` matches `<app>` and write the
    // result under `<project_root>/<app>/migrations/`. The `dir`
    // argument's parent is the project root so existing manage CLI
    // wiring (which passes the flat `migrations/` dir) keeps working.
    if let Some(app_name) = app {
        let project_root = dir.parent().unwrap_or(dir);
        match crate::migrate::make::make_migrations_for_app_with(project_root, &app_name, name.as_deref(), drops)? {
            Some(mig) => {
                let app_dir = project_root.join(&app_name).join("migrations");
                writeln!(w, "wrote {}", file_path(&app_dir, &mig.name).display())?;
                for op in &mig.forward {
                    writeln!(w, "    + {}", describe_op(op))?;
                }
            }
            None => writeln!(
                w,
                "app `{app_name}`: no changes — models match latest snapshot (or no models with this app_label)"
            )?,
        }
        return Ok(());
    }

    // Explicit scope flag — emit exactly one file in that scope.
    if let Some(scope) = scope_override {
        return write_scoped_migration(dir, scope, name.as_deref(), drops, w);
    }

    // Auto-detect: if any registered model is `scope = "registry"`,
    // we're in a tenancy project. Run BOTH scopes so framework
    // registry tables get their own file and user's tenant models
    // get theirs. Each scope is a no-op when nothing in that scope
    // changed; the user sees one or two "wrote ..." lines.
    if has_registry_models() {
        // Framework ("system app") migrations first — the framework's own
        // `rustango_*` tables are generated into `<project_root>/system/
        // migrations/` (registry + tenant scope), replacing the
        // hand-written bootstrap/ensure DDL. Silent when the framework
        // schema is unchanged (the common case for app developers).
        // Every file is planned first: a refused scope writes none.
        let run = crate::migrate::make::make_migrations_tenancy(dir, name.as_deref(), drops)?;
        let project_root = dir.parent().unwrap_or(dir);
        for (scope, m) in &run.system {
            writeln!(
                w,
                "wrote {} (system/{} scope)",
                file_path(&project_root.join("system").join("migrations"), &m.name).display(),
                scope.as_str(),
            )?;
            for op in &m.forward {
                writeln!(w, "    + {}", describe_op(op))?;
            }
        }
        for (scope, mig) in &run.app {
            match mig {
                Some(m) => {
                    writeln!(
                        w,
                        "wrote {} ({} scope)",
                        file_path(dir, &m.name).display(),
                        scope.as_str(),
                    )?;
                    for op in &m.forward {
                        writeln!(w, "    + {}", describe_op(op))?;
                    }
                }
                None => writeln!(w, "no changes for {} scope", scope.as_str(),)?,
            }
        }
        return Ok(());
    }

    // Single-tenant (no registry-scoped models): one migration covering
    // every registered model — the v0.24.x behavior.
    match crate::migrate::make::make_migrations_with(dir, name.as_deref(), drops)? {
        Some(mig) => {
            writeln!(w, "wrote {}", file_path(dir, &mig.name).display())?;
            for op in &mig.forward {
                writeln!(w, "    + {}", describe_op(op))?;
            }
        }
        None => writeln!(w, "no changes — registry matches latest snapshot")?,
    }
    Ok(())
}

/// A migrated registry model marks a tenancy project. Unmanaged ones
/// don't: `Translation` is registry-scoped in every build (#2360).
pub(super) fn has_registry_models() -> bool {
    inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .any(|e| e.schema.scope == crate::core::ModelScope::Registry && e.schema.managed)
}

pub(super) fn write_scoped_migration<W: Write>(
    dir: &Path,
    scope: crate::core::ModelScope,
    name: Option<&str>,
    drops: crate::migrate::make::DropAndAdd,
    w: &mut W,
) -> Result<(), MigrateError> {
    match crate::migrate::make::make_migrations_for_scope_with(dir, scope, name, drops)? {
        Some(mig) => {
            writeln!(
                w,
                "wrote {} ({} scope)",
                file_path(dir, &mig.name).display(),
                scope.as_str(),
            )?;
            for op in &mig.forward {
                writeln!(w, "    + {}", describe_op(op))?;
            }
        }
        None => writeln!(
            w,
            "no changes — {} models match latest snapshot",
            scope.as_str(),
        )?,
    }
    Ok(())
}

/// `makemigrations --merge` — issue #346. Reconcile divergent
/// migration history by writing an empty-forward marker file.
pub(super) fn run_merge<W: Write>(dir: &Path, w: &mut W) -> Result<(), MigrateError> {
    match crate::migrate::make::make_merge_migration(dir)? {
        Some(mig) => {
            writeln!(w, "wrote {}", file_path(dir, &mig.name).display())?;
            writeln!(
                w,
                "    merge node — empty `forward`, anchors the chain after divergent leaves"
            )?;
        }
        None => writeln!(
            w,
            "no merge needed — chain has at most one leaf (the history is already linear)"
        )?,
    }
    Ok(())
}

/// Write an empty migration scaffold (`forward: []`) carrying the
/// predecessor's snapshot so a subsequent `make_migrations` doesn't
/// re-emit the same diff. The user fills in `forward` by hand for
/// data migrations.
///
/// Public so binaries that want a programmatic equivalent of
/// `makemigrations --empty` can call it directly.
///
/// # Errors
/// As [`crate::migrate::file::list_dir`] / [`crate::migrate::file::write`].
pub fn make_empty(dir: &Path, name: &str) -> Result<Migration, MigrateError> {
    make_empty_in_scope(dir, name, crate::migrate::MigrationScope::default())
}

/// [`make_empty`] for `scope`: chains onto that scope's last file and copies
/// its snapshot, renames included, so the next diff starts from it.
///
/// # Errors
/// As [`make_empty`].
pub fn make_empty_in_scope(
    dir: &Path,
    name: &str,
    scope: crate::migrate::MigrationScope,
) -> Result<Migration, MigrateError> {
    write_scaffold(dir, name, scope, Vec::new())
}

/// A file holding `forward`, chained onto `scope`'s last file with its
/// effective snapshot, so a later diff in that scope starts from it.
pub(super) fn write_scaffold(
    dir: &Path,
    name: &str,
    scope: crate::migrate::MigrationScope,
    forward: Vec<Operation>,
) -> Result<Migration, MigrateError> {
    crate::migrate::require_v1_project(dir)?;
    let prior = file::list_dir(dir)?;
    let chain: Vec<&Migration> = prior.iter().filter(|m| m.scope == scope).collect();
    let head = chain.last().copied();
    let prev_snapshot = head.map_or_else(SchemaSnapshot::default, |m| {
        crate::migrate::make::effective_snapshot(&chain, m)
    });
    let prev_name = head.map(|m| m.name.clone());
    // Numbered across the whole dir, so the two scopes never share a name.
    let next_index = prior
        .last()
        .and_then(|m| file::extract_index(&m.name))
        .map_or(1, |n| n + 1);

    let full_name = format!("{next_index:04}_{name}");
    let mig = Migration {
        name: full_name.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        prev: prev_name,
        atomic: true,
        scope,
        replaces: Vec::new(),
        merges: Vec::new(),
        snapshot: prev_snapshot,
        forward,
    };
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    file::write(&file_path(dir, &mig.name), &mig)?;
    Ok(mig)
}

pub(super) fn file_path(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.json"))
}

// ------------------------------------------------------------------ add-data-op

/// Create a new migration containing a single data operation.
///
/// `name` is the migration name suffix (e.g. `"backfill_slugs"`); the index
/// prefix is derived automatically from the migration chain in `dir`.
///
/// If `reverse_sql` is `Some`, the op is marked `reversible = true` and
/// `unapply` / `downgrade` will run the reverse SQL. If `None`, the op is
/// irreversible and rollback will fail fast.
///
/// # Errors
/// [`MigrateError::Io`] / [`MigrateError::Json`] if the file can't be written.
pub fn make_data_migration(
    dir: &Path,
    name: &str,
    sql: &str,
    reverse_sql: Option<&str>,
) -> Result<Migration, MigrateError> {
    make_data_migration_in_scope(
        dir,
        name,
        sql,
        reverse_sql,
        crate::migrate::MigrationScope::default(),
    )
}

/// [`make_data_migration`] for `scope`: chains onto that scope's last file,
/// as [`make_empty_in_scope`] does.
///
/// # Errors
/// As [`make_data_migration`].
pub fn make_data_migration_in_scope(
    dir: &Path,
    name: &str,
    sql: &str,
    reverse_sql: Option<&str>,
    scope: crate::migrate::MigrationScope,
) -> Result<Migration, MigrateError> {
    let op = Operation::Data(DataOp {
        sql: sql.to_owned(),
        reverse_sql: reverse_sql.map(str::to_owned),
        reversible: reverse_sql.is_some(),
    });
    write_scaffold(dir, name, scope, vec![op])
}

/// Append a data operation to an existing migration file.
///
/// `migration_name` is the full migration name (e.g. `"0002_add_slug"`).
/// The op is appended to the end of `forward`. If `reverse_sql` is `Some`
/// the op is reversible.
///
/// # Errors
/// [`MigrateError::Validation`] if `migration_name` not found in `dir`.
/// [`MigrateError::Io`] / [`MigrateError::Json`] for file I/O failures.
pub fn append_data_op(
    dir: &Path,
    migration_name: &str,
    sql: &str,
    reverse_sql: Option<&str>,
) -> Result<(), MigrateError> {
    crate::migrate::require_v1_project(dir)?;
    let path = file_path(dir, migration_name);
    let mut mig = file::load(&path).map_err(|e| match e {
        MigrateError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            MigrateError::Validation(format!(
                "migration `{migration_name}` not found at {}",
                path.display()
            ))
        }
        other => other,
    })?;
    mig.forward.push(Operation::Data(DataOp {
        sql: sql.to_owned(),
        reverse_sql: reverse_sql.map(str::to_owned),
        reversible: reverse_sql.is_some(),
    }));
    file::write(&path, &mig)?;
    Ok(())
}

/// `add-data-op` subcommand handler.
pub(super) fn add_data_op_cmd<W: Write>(
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut sql: Option<String> = None;
    let mut reverse_sql: Option<String> = None;
    let mut name: Option<String> = None;
    let mut to: Option<String> = None;
    let mut scope: Option<crate::migrate::MigrationScope> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--scope" => {
                let raw = iter.next().ok_or_else(|| {
                    MigrateError::Validation("--scope requires \"registry\" or \"tenant\"".into())
                })?;
                scope = match crate::core::ModelScope::from_str(raw) {
                    Some(crate::core::ModelScope::Registry) => {
                        Some(crate::migrate::MigrationScope::Registry)
                    }
                    Some(crate::core::ModelScope::Tenant) => {
                        Some(crate::migrate::MigrationScope::Tenant)
                    }
                    None => {
                        return Err(MigrateError::Validation(format!(
                            "--scope must be \"registry\" or \"tenant\", got {raw:?}"
                        )))
                    }
                };
            }
            "--sql" => {
                sql = Some(iter.next().cloned().ok_or_else(|| missing_value("--sql"))?);
            }
            "--reverse-sql" => {
                reverse_sql = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| missing_value("--reverse-sql"))?,
                );
            }
            "--name" => {
                name = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| missing_value("--name"))?,
                );
            }
            "--to" => {
                to = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--to requires a migration name".into())
                })?);
            }
            "--help" | "-h" => {
                writeln!(
                    w,
                    "add-data-op --sql <SQL> [--reverse-sql <SQL>] [--name <name>] [--to <migration> | --scope <s>]"
                )?;
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unexpected argument: `{other}` — use --sql, --reverse-sql, --name, --to, --scope"
                )));
            }
        }
    }

    let sql = sql.ok_or_else(|| MigrateError::Validation("--sql is required".into()))?;

    if let Some(migration_name) = to {
        if scope.is_some() {
            return Err(MigrateError::Validation(
                "--to appends to that file; it does not combine with --scope".into(),
            ));
        }
        append_data_op(dir, &migration_name, &sql, reverse_sql.as_deref())?;
        writeln!(w, "appended data op to {migration_name}.json")?;
    } else {
        let name = name.unwrap_or_else(|| "data_op".to_owned());
        let mig = make_data_migration_in_scope(
            dir,
            &name,
            &sql,
            reverse_sql.as_deref(),
            scope.unwrap_or_default(),
        )?;
        let rev_note = if reverse_sql.is_some() {
            " (reversible)"
        } else {
            " (irreversible)"
        };
        writeln!(w, "wrote {}{rev_note}", file_path(dir, &mig.name).display())?;
    }
    Ok(())
}

pub(super) fn describe_op(op: &Operation) -> String {
    match op {
        Operation::Schema(c) => format!("{c:?}"),
        Operation::Data(d) => {
            let head: String = d.sql.chars().take(60).collect();
            let ellipsis = if d.sql.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            format!("data: {head}{ellipsis}")
        }
        Operation::Callback(c) => format!("callback: {}", c.name),
    }
}
