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
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // v0.38 — on PG the legacy `migrate_tenants` handles both schema-
    // mode and database-mode tenants; on non-PG we route through the
    // generic `migrate_tenants_db` which is database-mode-only by
    // design (schema-mode is PG-only by language).
    #[cfg(feature = "postgres")]
    let report = {
        if let Some(pg_pools) =
            (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
        {
            tenant_migrate::migrate_tenants(pg_pools, dir, registry_url).await?
        } else {
            tenant_migrate::migrate_tenants_db(pools, dir, registry_url).await?
        }
    };
    #[cfg(not(feature = "postgres"))]
    let report = tenant_migrate::migrate_tenants_db(pools, dir, registry_url).await?;
    write_tenant_report(w, &report)
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
    Ok(())
}

// ---------- migrate-registry ----------

pub(super) async fn migrate_registry_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
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
             migrate-registry                apply registry-scoped pending migrations only\n\
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
        // Forward the original args to the registry runner.
        let mut forwarded = vec!["migrate".to_owned()];
        forwarded.extend(args.iter().cloned());
        return rustango::migrate::manage::run_with_writer(
            &pools.registry_pool(),
            dir,
            forwarded,
            w,
        )
        .await
        .map_err(TenancyError::Migrate);
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

    // Tenant phase. Branch by backend: PG goes through the legacy
    // `migrate_tenants` (handles schema-mode + database-mode);
    // sqlite/mysql route through `migrate_tenants_db` (database-mode
    // only — schema-mode is PG-only by language).
    #[cfg(feature = "postgres")]
    let report = {
        if let Some(pg_pools) =
            (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
        {
            tenant_migrate::migrate_tenants(pg_pools, dir, registry_url).await?
        } else {
            tenant_migrate::migrate_tenants_db(pools, dir, registry_url).await?
        }
    };
    #[cfg(not(feature = "postgres"))]
    let report = tenant_migrate::migrate_tenants_db(pools, dir, registry_url).await?;
    write_tenant_report(w, &report)?;
    Ok(())
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
    Ok(())
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
                // `migrations/` — mirror `apply_system_migrations`.
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
