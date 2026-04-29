//! Migration verbs: `init-tenancy`, `migrate-registry`,
//! `migrate-tenants`, and the scope-aware fallback `migrate`.

use std::io::Write;
use std::path::Path;

use crate::tenancy::error::TenancyError;
use crate::tenancy::migrate as tenant_migrate;
use crate::tenancy::pools::TenantPools;

// ---------- migrate-tenants ----------

pub(super) async fn migrate_tenants_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    dir: &Path,
    w: &mut W,
) -> Result<(), TenancyError> {
    let report = tenant_migrate::migrate_tenants(pools, dir, registry_url).await?;
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

pub(super) async fn migrate_registry_cmd<W: Write + Send>(
    pools: &TenantPools,
    dir: &Path,
    w: &mut W,
) -> Result<(), TenancyError> {
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

pub(super) async fn migrate_all_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    // Pass any flags / args (e.g. `--dry-run`, `--help`, target name)
    // through to the registry-side runner. The single-tenant manage
    // runner doesn't know about scopes, so for now we let the
    // tenant phase short-circuit on `--help` / target args. Most
    // operators just type `migrate` with no args.
    let mut iter = args.iter();
    let mut help = false;
    let mut dry_run = false;
    let mut target: Option<&str> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--dry-run" => dry_run = true,
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
             migrate-registry                apply registry-scoped pending migrations only\n\
             migrate-tenants                 apply tenant-scoped pending migrations across active orgs"
        )?;
        return Ok(());
    }
    if target.is_some() || dry_run {
        // Targeted / dry-run mode is registry-only — tenant-scoped
        // routing for arbitrary targets isn't well-defined yet.
        // Forward the original args to the registry runner.
        let mut forwarded = vec!["migrate".to_owned()];
        forwarded.extend(args.iter().cloned());
        return rustango::migrate::manage::run_with_writer(pools.registry(), dir, forwarded, w)
            .await
            .map_err(TenancyError::Migrate);
    }

    // Registry phase.
    let registry_applied = tenant_migrate::migrate_registry(pools, dir).await?;
    if registry_applied.is_empty() {
        writeln!(w, "registry: nothing to migrate (already up to date)")?;
    } else {
        writeln!(w, "registry: applied {} migration(s)", registry_applied.len())?;
        for m in &registry_applied {
            writeln!(w, "  + {}", m.name)?;
        }
    }

    // Tenant phase.
    let report = tenant_migrate::migrate_tenants(pools, dir, registry_url).await?;
    write_tenant_report(w, &report)?;
    Ok(())
}

// ---------- init-tenancy ----------

pub(super) fn init_tenancy_cmd<W: Write>(dir: &Path, w: &mut W) -> Result<(), TenancyError> {
    let report = crate::tenancy::bootstrap::init_tenancy(dir)?;
    if report.written.is_empty() && report.skipped.is_empty() {
        // Should not happen — init_tenancy always processes both files.
        writeln!(w, "init-tenancy: no migrations to write")?;
        return Ok(());
    }
    writeln!(
        w,
        "init-tenancy: bootstrap migrations in {}",
        dir.display()
    )?;
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
