//! Migration verbs: `init-tenancy`, `migrate-registry`,
//! `migrate-tenants`, and the scope-aware fallback `migrate`.

use std::io::Write;
use std::path::Path;

use sqlx::Database;

use crate::tenancy::error::TenancyError;
use crate::tenancy::migrate as tenant_migrate;
use crate::tenancy::pools::TenantPools;

// ---------- migrate-tenants ----------

pub(super) async fn migrate_tenants_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // No `--dry-run` here: refusing it beats migrating every tenant (#1909).
    super::args::parse(
        args,
        &super::args::Spec {
            verb: "migrate-tenants",
            usage: "migrate-tenants   apply tenant-scoped pending migrations across active orgs",
            switches: &[],
            valued: &[],
            max_positionals: 0,
        },
    )?;
    // Progress goes to the same writer the report does, so `manage
    // migrate-tenants` shows each migration as it lands instead of
    // sitting silent for the length of the run.
    let progress = CliProgress::new(w);
    let report = tenant_migrate::migrate_tenants_dyn_with_progress(
        pools,
        dir,
        registry_url,
        Some(&progress),
    )
    .await?;
    let w = progress.into_inner();
    write_tenant_report(w, &report)
}

/// Prints tenant-migration progress to the verb's own writer.
///
/// ## Why the `Mutex`
///
/// An observer is `&self` and `Sync` — it has to be, because the runner
/// hands events out from inside an async body — while a verb writes
/// through `&mut W`. The mutex is what bridges those, and it is
/// uncontended: the runner emits from one task at a time.
///
/// ## On blocking
///
/// [`crate::migrate::progress`] says observers must not block, because
/// events are emitted with the migrate lock held. Writing a line to a
/// terminal is microseconds, and this is a CLI: one process, one
/// operator, and if it does stall on a full pipe (`manage migrate |
/// head -1`) the only migration it delays is the operator's own. That
/// reasoning does **not** transfer to a server-side observer.
struct CliProgress<'w, W> {
    out: std::sync::Mutex<&'w mut W>,
}

impl<'w, W: Write + Send> CliProgress<'w, W> {
    fn new(out: &'w mut W) -> Self {
        Self {
            out: std::sync::Mutex::new(out),
        }
    }

    /// Hand the writer back so the caller can print the final report.
    fn into_inner(self) -> &'w mut W {
        // A poisoned lock means an observer call panicked. The docs say
        // not to panic in one; if it happened anyway, recover the writer
        // rather than taking the whole verb down over progress output.
        self.out.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W: Write + Send> tenant_migrate::TenantMigrationObserver for CliProgress<'_, W> {
    fn on_event(&self, event: tenant_migrate::TenantMigrationEvent) {
        use crate::migrate::{MigrationEvent, Outcome};
        use tenant_migrate::{Chain, TenantMigrationEvent as E};

        let Ok(mut out) = self.out.lock() else {
            return;
        };
        // Progress is cosmetic — a broken pipe or a full disk must not
        // turn a successful migration into a failed verb, so every write
        // here is deliberately unchecked.
        let _ = match event {
            E::Planned { tenants } if tenants > 0 => {
                writeln!(out, "migrating {tenants} tenant(s)…")
            }
            // Nothing to print for either: a plan of zero tenants, and
            // the per-tenant summary `write_tenant_report` already
            // prints once the run is over.
            E::Planned { .. } | E::TenantFinished { .. } => Ok(()),
            E::TenantStarted { slug, index, total } => {
                writeln!(out, "[{index}/{total}] {slug}")
            }
            E::Migration { chain, event, .. } => {
                // The two chains number independently, so `0001_initial`
                // shows up in both. Tag which one, or it reads as a
                // migration that ran twice.
                let tag = match chain {
                    Chain::System => "system",
                    Chain::Project => "app",
                };
                match event {
                    MigrationEvent::Finished {
                        name,
                        outcome,
                        elapsed,
                        ..
                    } => {
                        let verb = match outcome {
                            Outcome::Ran => "applied",
                            Outcome::RanPartial { .. } => "applied (partial)",
                            Outcome::Faked => "faked",
                        };
                        writeln!(
                            out,
                            "        {verb} {tag}/{name} ({:.1}s)",
                            elapsed.as_secs_f64()
                        )
                    }
                    MigrationEvent::Failed { name, error, .. } => {
                        writeln!(out, "        ✗ {tag}/{name}: {error}")
                    }
                    // `Started` and `Planned` are deliberately quiet:
                    // one line per migration, printed when it lands and
                    // carrying its duration, beats two.
                    _ => Ok(()),
                }
            }
        };
        let _ = out.flush();
    }
}

fn write_tenant_report<W: Write>(
    w: &mut W,
    report: &crate::tenancy::migrate::TenantMigrationReport,
) -> Result<(), TenancyError> {
    if report.tenants.is_empty() {
        writeln!(w, "no active tenants")?;
        return Ok(());
    }
    writeln!(
        w,
        "ran tenant migrations against {} tenant(s); {} failure(s)",
        report.tenants.len(),
        report.failure_count(),
    )?;
    for o in &report.tenants {
        if let Some(err) = &o.error {
            writeln!(w, "  ✗ {}: {err}", o.slug)?;
        } else if o.applied.is_empty() {
            writeln!(w, "  · {}: up to date", o.slug)?;
        } else {
            writeln!(w, "  ✓ {}: {} migration(s)", o.slug, o.applied.len())?;
        }
    }
    tenant_failures(report.failure_count(), report.tenants.len())
}

/// Every tenant's applied project migrations, inactive tenants included.
/// A tenant that can't be read fails the whole call, so nothing is assumed (#2393).
pub(super) async fn tenant_ledgers<DB: Database>(
    pools: &TenantPools<DB>,
) -> Result<Vec<(String, std::collections::HashSet<String>)>, rustango::migrate::MigrateError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::sql::FetcherPool as _;
    use rustango::migrate::{MigrateError, LEDGER_TABLE};

    let unreadable = |slug: &str, e: &dyn std::fmt::Display| {
        MigrateError::Validation(format!(
            "could not read tenant `{slug}`'s migration ledger: {e}"
        ))
    };
    let orgs: Vec<crate::tenancy::org::Org> = crate::tenancy::org::Org::objects()
        .fetch(&pools.registry_pool())
        .await?;
    let mut out = Vec::with_capacity(orgs.len());
    for org in orgs {
        let (pool, own) = ledger_pool(pools, &org)
            .await
            .map_err(|e| unreadable(&org.slug, &e))?;
        let has_ledger = rustango::migrate::try_table_exists_here(&pool, LEDGER_TABLE)
            .await
            .map_err(|e| unreadable(&org.slug, &e))?;
        let applied = if has_ledger {
            rustango::migrate::applied_set_pool(&pool)
                .await
                .map_err(|e| unreadable(&org.slug, &e))?
        } else {
            std::collections::HashSet::new()
        };
        if own {
            pool.close().await;
        }
        out.push((org.slug, applied));
    }
    Ok(out)
}

/// The tenant's pool, but a SQLite file is opened with `mode=rw`, which
/// never creates it: a missing file is unreadable, not empty (#2393).
/// `true` when the pool is this call's own, to close.
async fn ledger_pool<DB: Database>(
    pools: &TenantPools<DB>,
    org: &crate::tenancy::org::Org,
) -> Result<(crate::sql::Pool, bool), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    if org.storage_mode == "database" && org.database_url.is_some() {
        let url = pools.resolved_database_url(org).await?;
        if url.starts_with("sqlite:") {
            let pool = crate::sql::Pool::connect(&sqlite_without_create(&url))
                .await
                .map_err(|e| TenancyError::Validation(e.to_string()))?;
            return Ok((pool, true));
        }
    }
    Ok((pools.scoped_pool_dyn(org).await?, false))
}

/// `url` with its `mode=` replaced by `rw`: open an existing file, never make one.
fn sqlite_without_create(url: &str) -> String {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut params: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("mode="))
        .collect();
    params.push("mode=rw");
    format!("{base}?{}", params.join("&"))
}

#[cfg(test)]
mod sqlite_without_create_tests {
    use super::sqlite_without_create;

    #[test]
    fn any_mode_becomes_rw() {
        assert_eq!(
            sqlite_without_create("sqlite://a.db"),
            "sqlite://a.db?mode=rw"
        );
        assert_eq!(
            sqlite_without_create("sqlite://a.db?mode=rwc&journal_mode=wal"),
            "sqlite://a.db?journal_mode=wal&mode=rw"
        );
    }
}

/// A non-zero exit for any failed tenant, so a deploy can't go on half-migrated (#1844).
pub(super) fn tenant_failures(failed: usize, total: usize) -> Result<(), TenancyError> {
    if failed == 0 {
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "{failed} of {total} tenant(s) failed; see the report above"
    )))
}

// ---------- migrate-registry ----------

pub(super) async fn migrate_registry_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let parsed = super::args::parse(
        args,
        &super::args::Spec {
            verb: "migrate-registry",
            usage: "migrate-registry [--dry-run]   apply (or preview) registry-scoped pending migrations",
            switches: &["--dry-run"],
            valued: &[],
            max_positionals: 0,
        },
    )?;
    if parsed.has("--dry-run") {
        return run_registry_scoped(pools, dir, &["--dry-run".to_owned()], w).await;
    }
    let applied = tenant_migrate::migrate_registry(pools, dir).await?;
    if applied.is_empty() {
        writeln!(w, "registry: nothing to migrate (already up to date)")?;
    } else {
        writeln!(w, "registry: applied {} migration(s)", applied.len())?;
        for m in &applied {
            writeln!(w, "  + {}", m.name)?;
        }
    }
    Ok(())
}

// ---------- migrate (scope-aware) ----------

pub(super) async fn migrate_all_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // Pass any flags / args (e.g. `--dry-run`, `--help`, target name)
    // through to the registry-side runner. The single-tenant manage
    // runner doesn't know about scopes, so for now we let the
    // tenant phase short-circuit on `--help` / target args. Most
    // operators just type `migrate` with no args.
    let mut iter = args.iter();
    let mut help = false;
    let mut dry_run = false;
    let mut target: Option<&str> = None;
    // v0.27.4 (#64) — `--fake <name>` backfills a ledger row
    // without running the migration SQL. Recovery path for the
    // "tables exist but ledger doesn't know" drift that surfaces
    // as `relation "X" already exists` (Postgres 42P07) on the
    // next `migrate` attempt. Multiple `--fake` flags accumulate
    // so operators can repair a stretch of drifted rows in one
    // command.
    let mut fakes: Vec<String> = Vec::new();
    // Which chain the fake stamps into, and where it runs. `--fake` alone
    // means "the project's migrations, in the registry DB" (the historical
    // behavior); `--system` switches to the framework's own chain and
    // `--all-tenants` fans the stamp out across every active tenant.
    let mut fake_scope = FakeScope::Project;
    let mut fake_all_tenants = false;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--dry-run" => dry_run = true,
            "--system" => fake_scope = FakeScope::System,
            "--all-tenants" => fake_all_tenants = true,
            "--fake" => {
                let name = iter.next().ok_or_else(|| {
                    TenancyError::Migrate(rustango::migrate::MigrateError::Validation(
                        "--fake requires a migration name (e.g. `--fake 0001_rustango_registry_initial`)".into(),
                    ))
                })?;
                fakes.push(name.clone());
            }
            other if other.starts_with('-') => {
                return Err(TenancyError::Migrate(
                    rustango::migrate::MigrateError::Validation(format!(
                        "unknown migrate flag: {other}"
                    )),
                ));
            }
            other => {
                if target.is_some() {
                    return Err(TenancyError::Migrate(
                        rustango::migrate::MigrateError::Validation(format!(
                            "unexpected positional argument: {other}"
                        )),
                    ));
                }
                target = Some(other);
            }
        }
    }
    if help {
        writeln!(
            w,
            "migrate                         apply registry-scoped + every tenant's pending migrations\n\
             migrate <target>                forward or back to <target> (registry-scoped only — use migrate-tenants for tenants)\n\
             migrate --dry-run               preview SQL for registry-scoped pending migrations\n\
             migrate --fake <name>           insert <name> into the registry ledger WITHOUT running its SQL\n\
                                             (recovery path when tables exist but the ledger row is missing — fixes\n\
                                             \"relation X already exists\" 42P07 errors after a manual setup)\n\
             migrate --fake <name> --system  stamp the framework's system-migration chain instead of the project's\n\
             migrate --fake <name> --all-tenants\n\
                                             stamp every active tenant's ledger rather than the registry\n\
                                             (combine with --system for the framework's own tables)\n\
             migrate-registry [--dry-run]    apply (or preview) registry-scoped pending migrations only\n\
             migrate-tenants                 apply tenant-scoped pending migrations across active orgs"
        )?;
        return Ok(());
    }
    if !fakes.is_empty() {
        let (fake_dir, ledger) = fake_scope.resolve(dir);
        if fake_all_tenants {
            return fake_apply_across_tenants(pools, &fake_dir, ledger, &fakes, w).await;
        }
        if fake_scope == FakeScope::System {
            return fake_apply_to_pool(
                &pools.registry_pool(),
                &fake_dir,
                ledger,
                &fakes,
                "registry (system chain)",
                w,
            )
            .await;
        }
        return fake_apply_to_registry(pools, dir, &fakes, w).await;
    }
    if target.is_some() || dry_run {
        // Targeted / dry-run mode is registry-only — tenant-scoped
        // routing for arbitrary targets isn't well-defined yet.
        return run_registry_scoped(pools, dir, args, w).await;
    }

    // Registry phase.
    let registry_applied = tenant_migrate::migrate_registry(pools, dir).await?;
    if registry_applied.is_empty() {
        writeln!(w, "registry: nothing to migrate (already up to date)")?;
    } else {
        writeln!(
            w,
            "registry: applied {} migration(s)",
            registry_applied.len()
        )?;
        for m in &registry_applied {
            writeln!(w, "  + {}", m.name)?;
        }
    }

    // Tenant phase, through the one dispatch seam — see
    // `migrate_tenants_dyn_with_progress` for why this is not a
    // backend branch written out here.
    let progress = CliProgress::new(w);
    let report = tenant_migrate::migrate_tenants_dyn_with_progress(
        pools,
        dir,
        registry_url,
        Some(&progress),
    )
    .await?;
    let w = progress.into_inner();
    write_tenant_report(w, &report)
}

/// Run the single-tenant `migrate` runner with `args` against the registry,
/// over the registry-scoped migrations only: the whole dir would put
/// tenant tables in the registry (#1909).
async fn run_registry_scoped<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use rustango::migrate::MigrationScope;
    if let Some(target) = args.iter().find(|a| !a.starts_with('-')) {
        let all = rustango::migrate::file::list_dir(dir)?;
        if all
            .iter()
            .any(|m| &m.name == target && m.scope == MigrationScope::Tenant)
        {
            return Err(TenancyError::Validation(format!(
                "`{target}` is tenant-scoped — `migrate <target>` moves the registry only; \
                 run `migrate-tenants`"
            )));
        }
    }
    let scoped = tenant_migrate::scoped_subset(dir, MigrationScope::Registry).await?;
    let mut forwarded = vec!["migrate".to_owned()];
    forwarded.extend(args.iter().cloned());
    rustango::migrate::manage::run_with_writer(
        &pools.registry_pool(),
        scoped.path(dir),
        forwarded,
        w,
    )
    .await
    .map_err(TenancyError::Migrate)
}

// ---------- init-tenancy ----------

pub(super) fn init_tenancy_cmd_with<W: Write>(
    dir: &Path,
    w: &mut W,
    init_fn: super::InitTenancyFn,
) -> Result<(), TenancyError> {
    let report = init_fn(dir)?;
    if report.written.is_empty() && report.skipped.is_empty() {
        // Should not happen — init_tenancy always processes both files.
        writeln!(w, "init-tenancy: no migrations to write")?;
        return Ok(());
    }
    writeln!(w, "init-tenancy: bootstrap migrations in {}", dir.display())?;
    for name in &report.written {
        writeln!(w, "  + wrote {name}.json")?;
    }
    for name in &report.skipped {
        writeln!(w, "  · {name}.json already exists — left untouched")?;
    }
    if !report.written.is_empty() {
        writeln!(w, "next: run `migrate` to apply them.")?;
    }
    Ok(())
}

// ---------- migrate --fake ---------- (#64)

/// Backfill the registry ledger with `names` without running any SQL.
/// Recovery path for the "tables exist but the ledger row is missing"
/// drift that surfaces as `relation "X" already exists` (Postgres
/// 42P07) on the next `migrate` attempt — common when the registry
/// DB was set up out-of-band, the ledger table was dropped, or a
/// previous migration partially succeeded.
///
/// Each `name` is validated against the migration directory before
/// the row lands so operators can't backfill a typo. The ledger
/// schema is created if missing (same shape as `ensure_ledger`).
async fn fake_apply_to_registry<W: Write, DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    names: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    fake_apply_to_pool(
        &registry,
        dir,
        rustango::migrate::LEDGER_TABLE,
        names,
        "registry",
        w,
    )
    .await
}

/// Stamp `names` into `ledger` for **every active tenant**.
///
/// The framework's own tables live per tenant, so repairing a drifted
/// system-migration ledger (or squash bookkeeping) is a per-tenant job. Each
/// tenant is processed independently and a failure is reported without
/// aborting the rest — the same failure-isolation policy
/// [`crate::tenancy::migrate::migrate_tenants`] uses, so one broken tenant
/// can't leave the others unrepaired.
async fn fake_apply_across_tenants<W: Write, DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    ledger: &str,
    names: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    use crate::tenancy::org::Org;

    let registry = pools.registry_pool();
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&registry)
        .await
        .map_err(|e| {
            TenancyError::Validation(format!("--fake --all-tenants: listing orgs failed: {e}"))
        })?;
    if orgs.is_empty() {
        writeln!(w, "no active tenants — nothing to stamp.")?;
        return Ok(());
    }
    let mut failures = 0;
    for org in &orgs {
        writeln!(w, "tenant `{}`:", org.slug)?;
        let pool = match pools.scoped_pool_dyn(org).await {
            Ok(p) => p,
            Err(e) => {
                failures += 1;
                writeln!(w, "  ! could not open a pool: {e}")?;
                continue;
            }
        };
        if let Err(e) = fake_apply_to_pool(&pool, dir, ledger, names, &org.slug, w).await {
            failures += 1;
            writeln!(w, "  ! {e}")?;
        }
    }
    writeln!(
        w,
        "stamped {} tenant(s); {failures} failure(s).",
        orgs.len()
    )?;
    tenant_failures(failures, orgs.len())
}

/// Which migration chain a `--fake` targets.
///
/// The framework keeps its own tables in a **separate** chain (generated
/// `system/migrations/`, recorded in `__rustango_system_migrations__`) from
/// the project's (`migrations/`, `__rustango_migrations__`), so stamping a
/// row needs to know which pair to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FakeScope {
    /// The project's own migrations.
    Project,
    /// The framework's system-app migrations.
    System,
}

impl FakeScope {
    /// The `(directory, ledger)` pair this scope stamps into, given the
    /// project's migrations dir.
    fn resolve(self, dir: &Path) -> (std::path::PathBuf, &'static str) {
        match self {
            Self::Project => (dir.to_path_buf(), rustango::migrate::LEDGER_TABLE),
            Self::System => {
                // `system/migrations/` sits beside the project's
                // `migrations/` — mirror `SystemChain::for_migrations_dir`.
                let root = if dir.file_name().and_then(|n| n.to_str()) == Some("migrations") {
                    dir.parent().unwrap_or(dir)
                } else {
                    dir
                };
                (
                    root.join("system").join("migrations"),
                    crate::tenancy::migrate::SYSTEM_LEDGER,
                )
            }
        }
    }
}

/// Backfill `ledger` in `pool` with `names` without running any SQL.
///
/// Recovery path for the "tables exist but the ledger row is missing" drift
/// that surfaces as `relation "X" already exists` (Postgres 42P07) /
/// `table already exists` (MySQL 1050) on the next `migrate` — a DB set up
/// out-of-band, a dropped ledger, a partially-succeeded migration, or a
/// subsystem whose tables predate its migration.
///
/// Each name is validated against `dir` before the row lands, so operators
/// can't backfill a typo. The ledger is created if missing. `label` names
/// the target in the output (`registry`, a tenant slug, …).
async fn fake_apply_to_pool<W: Write>(
    pool: &crate::sql::Pool,
    dir: &Path,
    ledger: &str,
    names: &[String],
    label: &str,
    w: &mut W,
) -> Result<(), TenancyError> {
    // Discover what's on disk to validate the names.
    let migrations = rustango::migrate::file::list_dir(dir).map_err(TenancyError::Migrate)?;
    let on_disk: std::collections::HashSet<&str> =
        migrations.iter().map(|m| m.name.as_str()).collect();
    for name in names {
        if !on_disk.contains(name.as_str()) {
            return Err(TenancyError::Migrate(
                rustango::migrate::MigrateError::Validation(format!(
                    "--fake: no migration named `{name}` in {} \
                     (run `showmigrations` to list available names)",
                    dir.display()
                )),
            ));
        }
    }

    // Ensure the ledger table exists, then INSERT each row idempotently.
    // v0.38 — route through the tri-dialect `_pool` helpers + the
    // dialect's `placeholder(n)` emitter so the same code works on
    // PG (`$1`) and sqlite/mysql (`?`).
    rustango::migrate::ensure_ledger_pool_with_ledger(pool, ledger)
        .await
        .map_err(TenancyError::Migrate)?;
    let sql = {
        let dialect = pool.dialect();
        let placeholder = dialect.placeholder(1);
        let table = dialect.quote_ident(ledger);
        let name_col = dialect.quote_ident("name");
        let conflict_tail = dialect.insert_on_conflict_skip(&[&name_col]);
        format!("INSERT INTO {table} ({name_col}) VALUES ({placeholder}) {conflict_tail}")
    };
    for name in names {
        let affected = rustango::sql::raw_execute_pool(
            pool,
            &sql,
            vec![rustango::core::SqlValue::String(name.clone())],
        )
        .await
        .map_err(|e| {
            TenancyError::Migrate(rustango::migrate::MigrateError::Validation(format!(
                "--fake: insert into ledger failed for `{name}`: {e}"
            )))
        })?;
        if affected == 0 {
            writeln!(w, "  · {name} already in ledger — left untouched")?;
        } else {
            writeln!(w, "  + faked {name} (no SQL run; ledger row inserted)")?;
        }
    }
    writeln!(
        w,
        "{label}: {} fake row(s) processed. Run `migrate` to apply any actually-pending migrations.",
        names.len()
    )?;
    Ok(())
}
