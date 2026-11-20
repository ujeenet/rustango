//! `check`: database and pending-migration audits.

use super::*;

/// Warn when the database's default collation ignores case (#1742).
pub(super) async fn collation_audit(pool: &Pool, audit: &mut DeployAuditFindings) {
    let Some(sql) = pool.dialect().default_collation_sql() else {
        return;
    };
    match crate::sql::raw_query_pool::<(String,)>(sql, Vec::new(), pool).await {
        Ok(rows) => {
            if let Some(w) = rows.first().and_then(|(c,)| collation_warning(c)) {
                audit.warnings.push(w);
            }
        }
        Err(e) => audit
            .warnings
            .push(format!("could not read the database collation: {e}")),
    }
}

/// Warn when named time zones convert to NULL, as `trunc_with_tz` then does.
pub(super) async fn time_zone_audit(pool: &Pool, audit: &mut DeployAuditFindings) {
    let Some(sql) = pool.dialect().named_time_zone_sql() else {
        return;
    };
    match crate::sql::raw_query_pool::<(Option<String>,)>(sql, Vec::new(), pool).await {
        Ok(rows) => {
            let converted = rows.first().and_then(|(c,)| c.as_deref());
            audit.warnings.extend(time_zone_warning(converted));
        }
        Err(e) => audit
            .warnings
            .push(format!("could not check named time zones: {e}")),
    }
}

pub(super) fn time_zone_warning(converted: Option<&str>) -> Option<String> {
    converted.is_none().then(|| {
        "the database has no time zone tables: `trunc_with_tz` with a named zone returns \
         NULL — load them with `mysql_tzinfo_to_sql`, or pass `±HH:MM` offsets"
            .to_owned()
    })
}

pub(super) fn collation_warning(collation: &str) -> Option<String> {
    const FIX: &str = "create the database with `COLLATE utf8mb4_0900_as_cs` \
                       (see the MySQL section of docs/getting-started.md)";
    if collation.ends_with("_ci") {
        Some(format!(
            "database default collation `{collation}` ignores case: `=` and `unique` on \
             text columns differ from PostgreSQL/SQLite — {FIX}"
        ))
    } else if collation.ends_with("_bin") {
        // MySQL flags `_bin` columns BINARY, and sqlx then refuses `String`.
        Some(format!(
            "database default collation `{collation}` makes text columns unreadable as \
             `String` — {FIX}"
        ))
    } else {
        None
    }
}

/// Warn about bare-admin SSO providers that refuse every admin user (#2359)
/// or weaken sign-in.
#[cfg(feature = "admin-sso")]
pub(super) async fn sso_link_audit(pool: &Pool, audit: &mut DeployAuditFindings) {
    match crate::sso::check::admin_providers(pool).await {
        Ok(slugs) => audit
            .warnings
            .extend(slugs.iter().map(|s| crate::sso::check::admin_warning(s))),
        Err(e) => audit.warnings.push(format!(
            "[sso] could not check the admin SSO providers: {e}"
        )),
    }
    match crate::sso::check::admin_risks(pool).await {
        Ok(found) => audit.warnings.extend(
            found
                .iter()
                .map(|r| crate::sso::check::risk_warning("admin provider", r)),
        ),
        Err(e) => audit.warnings.push(format!(
            "[sso] could not check the admin SSO providers: {e}"
        )),
    }
}

/// Each tenant's applied migrations, or why its ledgers could not be read.
pub(crate) type TenantApplied = Vec<(String, Result<TenantNames, String>)>;

/// One tenant's applied names: the project ledger's and the system ledger's.
pub(crate) struct TenantNames {
    pub(crate) project: std::collections::HashSet<String>,
    pub(crate) system: std::collections::HashSet<String>,
}

/// What a tenancy project adds to `check`: its deploy findings, awaited only
/// under `--deploy`, and the tenant ledgers, read only for tenant-scoped files.
pub(crate) struct TenancyCheck<D, L> {
    pub(crate) deploy: D,
    pub(crate) ledgers: L,
}

/// Tenants named in full before the rest are only counted.
pub(super) const TENANTS_NAMED: usize = 10;

/// The project's migration dirs, labelled for messages: `dir`, plus each
/// `<app>/migrations/` beside it when `apps` (tenancy applies those too).
pub(super) fn check_dirs(dir: &Path, apps: bool) -> Vec<(String, std::path::PathBuf)> {
    if !apps {
        return if dir.exists() {
            vec![(String::new(), dir.to_path_buf())]
        } else {
            Vec::new()
        };
    }
    let root = file::project_root_of(dir);
    file::project_migration_dirs(dir)
        .into_iter()
        .filter(|d| d.exists())
        .map(|d| {
            let app = d
                .parent()
                .filter(|p| *p != root && d != dir)
                .and_then(|p| p.file_name())
                .map(|n| format!("{}/", n.to_string_lossy()))
                .unwrap_or_default();
            (app, d)
        })
        .collect()
}

/// `files`' names in `scope` (any when `None`) that `applied` lacks.
pub(super) fn pending_names(
    files: &[(String, Vec<crate::migrate::Migration>)],
    applied: &std::collections::HashSet<String>,
    scope: Option<crate::migrate::MigrationScope>,
) -> Vec<String> {
    files
        .iter()
        .flat_map(|(app, migs)| {
            runner::migration_states(migs, applied)
                .filter(|(m, s)| {
                    *s == runner::MigrationState::Pending && scope.is_none_or(|sc| m.scope == sc)
                })
                .map(move |(m, _)| format!("{app}{}", m.name))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The ledgers `check` compares the files with.
pub(super) enum Ledgers<L> {
    /// One database: every file against its ledger.
    Single,
    /// Registry-scoped files against the registry; tenant-scoped ones against
    /// each tenant's, unless `--no-tenants` left `tenants` out.
    Tenancy { tenants: Option<L> },
}

/// Model changes no file holds yet, and files the registry's or a tenant's
/// ledger has not applied. `system` is the framework chain and the scope
/// `pool` runs of it; tenants run its tenant steps.
pub(super) async fn pending_migration_audit<L>(
    pool: &Pool,
    dir: &Path,
    files: &[(String, Vec<crate::migrate::Migration>)],
    (system, system_scope): (
        &[(String, Vec<crate::migrate::Migration>)],
        crate::migrate::MigrationScope,
    ),
    ledgers: Ledgers<L>,
    findings: (&mut Vec<String>, &mut Vec<String>, &mut Vec<String>),
) where
    L: std::future::Future<Output = Result<TenantApplied, String>>,
{
    let (info, warnings, errors) = findings;
    match crate::migrate::make::planned_migrations(dir, has_registry_models()) {
        Ok(planned) if !planned.is_empty() => {
            let ops: Vec<String> = planned
                .iter()
                .flat_map(|m| m.forward.iter().map(describe_op))
                .collect();
            warnings.push(format!(
                "models have changes no migration holds — run `manage makemigrations`:\n            {}",
                ops.join("\n            ")
            ));
        }
        Ok(_) => {}
        Err(e) => errors.push(format!("makemigrations would fail: {e}")),
    }
    // In tenancy the registry ledger records registry-scoped files only.
    let (scope, tenants) = match ledgers {
        Ledgers::Single => (None, None),
        Ledgers::Tenancy { tenants } => (
            Some(crate::migrate::MigrationScope::Registry),
            Some(tenants),
        ),
    };
    match runner::applied_or_empty(pool, runner::LEDGER_TABLE).await {
        Ok(applied) => {
            let pending = pending_names(files, &applied, scope);
            if !pending.is_empty() {
                warnings.push(format!(
                    "{} migration(s) not applied — run `manage migrate`: {}",
                    pending.len(),
                    pending.join(", ")
                ));
            }
        }
        Err(e) => errors.push(format!("cannot read the migration ledger: {e}")),
    }
    if !system.is_empty() {
        match runner::applied_or_empty(pool, runner::SYSTEM_LEDGER_TABLE).await {
            Ok(applied) => {
                let pending = pending_names(system, &applied, Some(system_scope));
                if !pending.is_empty() {
                    warnings.push(format!(
                        "{} system migration(s) not applied — run `manage migrate`: {}",
                        pending.len(),
                        pending.join(", ")
                    ));
                }
            }
            Err(e) => errors.push(format!("cannot read the system migration ledger: {e}")),
        }
    }
    let tenant_files = files
        .iter()
        .chain(system)
        .flat_map(|(_, m)| m)
        .any(|m| m.scope == crate::migrate::MigrationScope::Tenant);
    let Some(tenants) = tenants.filter(|_| tenant_files) else {
        return;
    };
    let Some(ledgers) = tenants else {
        info.push("tenant ledgers not read (--no-tenants)".into());
        return;
    };
    match ledgers.await {
        Ok(tenants) => tenant_pending_audit(files, system, &tenants, warnings),
        Err(e) => errors.push(format!("cannot list the tenants: {e}")),
    }
}

/// One warning naming the tenants behind on tenant-scoped files, the
/// framework's `system` ones included, one for those whose ledgers could not be read.
pub(super) fn tenant_pending_audit(
    files: &[(String, Vec<crate::migrate::Migration>)],
    system: &[(String, Vec<crate::migrate::Migration>)],
    tenants: &TenantApplied,
    warnings: &mut Vec<String>,
) {
    let tenant = Some(crate::migrate::MigrationScope::Tenant);
    let mut behind = Vec::new();
    let mut unreadable = Vec::new();
    for (slug, applied) in tenants {
        match applied {
            Ok(applied) => {
                let mut pending = pending_names(system, &applied.system, tenant);
                pending.extend(pending_names(files, &applied.project, tenant));
                if !pending.is_empty() {
                    behind.push(format!("{slug}: {}", pending.join(", ")));
                }
            }
            Err(e) => unreadable.push(format!("{slug}: {e}")),
        }
    }
    let list = |lines: &[String]| {
        let mut shown: Vec<&str> = lines
            .iter()
            .take(TENANTS_NAMED)
            .map(String::as_str)
            .collect();
        let more = lines.len().saturating_sub(TENANTS_NAMED);
        let tail = format!("… and {more} more");
        if more > 0 {
            shown.push(&tail);
        }
        shown.join("\n            ")
    };
    if !behind.is_empty() {
        warnings.push(format!(
            "{} tenant(s) behind on tenant migrations — run `manage migrate-tenants`:\n            {}",
            behind.len(),
            list(&behind)
        ));
    }
    if !unreadable.is_empty() {
        warnings.push(format!(
            "could not read {} tenant ledger(s):\n            {}",
            unreadable.len(),
            list(&unreadable)
        ));
    }
}

/// `manage check [--deploy]` — run system audits.
pub(super) async fn check_cmd<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    type Never<T> = std::future::Ready<T>;
    check_cmd_with(
        pool,
        dir,
        args,
        w,
        None::<TenancyCheck<Never<DeployAuditFindings>, Never<Result<TenantApplied, String>>>>,
    )
    .await
}

/// [`check_cmd`] for the tenancy dispatcher: `tenancy` adds its deploy
/// findings and the tenant ledgers, scans app dirs and turns on the apex row.
pub(crate) async fn check_cmd_with<W: Write, D, L>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
    tenancy: Option<TenancyCheck<D, L>>,
) -> Result<(), MigrateError>
where
    D: std::future::Future<Output = DeployAuditFindings>,
    L: std::future::Future<Output = Result<TenantApplied, String>>,
{
    // `--deplyo` once ran the plain check and reported all clear.
    let flags: &[&str] = if tenancy.is_some() {
        &["--deploy", "--no-tenants"]
    } else {
        &["--deploy"]
    };
    if let Some(other) = args.iter().find(|a| !flags.contains(&a.as_str())) {
        let usage = flags.iter().map(|f| format!(" [{f}]")).collect::<String>();
        return Err(MigrateError::Validation(format!(
            "check: unknown argument `{other}` (usage: check{usage})"
        )));
    }
    // Every active tenant's ledger is read; a slow tenant costs at most a few seconds.
    let read_tenants = !args.iter().any(|a| a == "--no-tenants");
    let (deploy_findings, ledgers) = match tenancy {
        Some(t) => (
            Some(t.deploy),
            Ledgers::Tenancy {
                tenants: read_tenants.then_some(t.ledgers),
            },
        ),
        None => (None, Ledgers::Single),
    };
    let deploy = args.iter().any(|a| a == "--deploy");
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut info: Vec<String> = Vec::new();

    writeln!(
        w,
        "running rustango system check{}...",
        if deploy { " (deploy mode)" } else { "" }
    )?;

    // Always-on checks
    let model_count = crate::core::inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .count();
    if model_count == 0 {
        errors.push("no models registered — every #[derive(Model)] struct must be `pub use`d through the binary's crate root".into());
    } else {
        info.push(format!("{model_count} models registered via inventory"));
    }

    // DB connectivity — tri-dialect via raw_execute_pool.
    if crate::sql::raw_execute_pool(pool, "SELECT 1", Vec::new())
        .await
        .is_err()
    {
        errors.push("cannot connect to database — verify DATABASE_URL is reachable".into());
    } else {
        info.push("database reachable".into());
    }

    // Pending migrations
    let dirs = check_dirs(dir, matches!(ledgers, Ledgers::Tenancy { .. }));
    if !dirs.is_empty() {
        let mut files = Vec::with_capacity(dirs.len());
        for (app, d) in dirs {
            files.push((app, file::list_dir(&d)?));
        }
        // The framework chain runs here and, in tenancy, in each tenant too;
        // its own ledger records it.
        let view = match &ledgers {
            Ledgers::Tenancy { .. } => SystemView::registry(dir),
            Ledgers::Single => SystemView::tenant(dir),
        };
        let system = view
            .files()?
            .map(|m| vec![("system/".to_owned(), m)])
            .unwrap_or_default();
        let count: usize = files.iter().map(|(_, m)| m.len()).sum();
        if count == 0 && model_count > 0 {
            warnings.push(
                "models registered but no migrations on disk — run `manage makemigrations`".into(),
            );
        } else {
            info.push(format!("{count} migration(s) on disk"));
            let findings = (&mut info, &mut warnings, &mut errors);
            let system = (system.as_slice(), view.scope);
            pending_migration_audit(pool, dir, &files, system, ledgers, findings).await;
        }
    }

    // Deploy checks
    if deploy {
        let mut audit = DeployAuditFindings::default();
        run_deploy_audit(&deploy_audit_env(deploy_findings.is_some()), &mut audit);
        // Settings audit (#87 slice 4) — flags dev-defaults left in
        // prod (e.g. `headers_preset = "dev"` in `prod_settings.toml`).
        // Gated by the `config` feature; no-op without it.
        #[cfg(feature = "config")]
        run_settings_audit(&mut audit);
        #[cfg(feature = "cache")]
        login_store_audit(crate::account_lockout::shared(), &mut audit);
        #[cfg(feature = "casts")]
        audit.errors.extend(crate::casts::sealed_key_problem());
        collation_audit(pool, &mut audit).await;
        time_zone_audit(pool, &mut audit).await;
        #[cfg(feature = "admin-sso")]
        sso_link_audit(pool, &mut audit).await;
        if let Some(extra) = deploy_findings {
            let extra = extra.await;
            audit.info.extend(extra.info);
            audit.warnings.extend(extra.warnings);
            audit.errors.extend(extra.errors);
        }
        // `required_db_vendor` + `required_db_features`
        // audit — every model declaring `required_db_vendor = "postgres"`
        // or `required_db_features = "json_path, listen_notify"` gets
        // compared against the active pool's dialect. Mismatches surface
        // as warnings so ops catches "I forgot to switch DATABASE_URL"
        // (and "this model wants LISTEN/NOTIFY but you're on SQLite")
        // at deploy time rather than the first runtime hit.
        let dialect = pool.dialect();
        let active = dialect.name();
        for entry in crate::core::inventory::iter::<crate::core::ModelEntry>.into_iter() {
            if let Some(want) = entry.schema.required_db_vendor {
                if want != active {
                    audit.warnings.push(format!(
                        "model `{}` declares `required_db_vendor = \"{want}\"` \
                         but the active database backend is `{active}` — \
                         queries that depend on backend-specific features may fail",
                        entry.schema.name,
                    ));
                }
            }
            for token in entry.schema.required_db_features {
                if !dialect.supports(token) {
                    audit.warnings.push(format!(
                        "model `{}` declares `required_db_features` token `{token}` \
                         which the active `{active}` dialect does not advertise — \
                         queries depending on it will fail at runtime",
                        entry.schema.name,
                    ));
                }
            }
        }
        info.extend(audit.info);
        warnings.extend(audit.warnings);
        errors.extend(audit.errors);
    }

    // Render
    for msg in &info {
        writeln!(w, "  [info]    {msg}")?;
    }
    for msg in &warnings {
        writeln!(w, "  [warning] {msg}")?;
    }
    for msg in &errors {
        writeln!(w, "  [error]   {msg}")?;
    }

    if !errors.is_empty() {
        return Err(MigrateError::Validation(format!(
            "{} system check(s) failed",
            errors.len()
        )));
    }
    if warnings.is_empty() && errors.is_empty() {
        writeln!(w, "all checks passed")?;
    }
    Ok(())
}
