//! `manage dbmigrate` under tenancy (11.1, 8.3): `apply`, `down`, `status`,
//! `check --drift` and `sql` work per database, the registry and each tenant.

use std::io::Write;
use std::path::Path;

use sqlx::Database;

use crate::migrate::v2::cli::{Back, Fleet, Out};
use crate::migrate::v2::run::{self, Options, RunAs};
use crate::migrate::v2::state::{Backend, Target};
use crate::migrate::MigrateError;
use crate::tenancy::error::TenancyError;
use crate::tenancy::migrate_v2::{self as fleet, FleetOptions, Tenants};
use crate::tenancy::pools::TenantPools;

/// The fleet of one tenancy CLI call.
pub(super) struct FleetCli<'a, DB: Database> {
    pub(super) pools: &'a TenantPools<DB>,
    pub(super) registry_url: &'a str,
}

fn back_to_migrate(e: TenancyError) -> MigrateError {
    match e {
        TenancyError::Migrate(e) => e,
        // The dispatcher wraps it again: one prefix, not two.
        TenancyError::Validation(msg) => MigrateError::Validation(msg),
        other => MigrateError::Validation(other.to_string()),
    }
}

impl<DB: Database> Fleet for FleetCli<'_, DB>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    fn registry(&self) -> crate::sql::Pool {
        self.pools.registry_pool()
    }

    async fn apply(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError> {
        apply(self, dir, args, w).await.map_err(back_to_migrate)
    }

    async fn down(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError> {
        down(self, dir, args, w).await.map_err(back_to_migrate)
    }

    async fn status(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError> {
        status(self, dir, args, w).await.map_err(back_to_migrate)
    }

    async fn drift(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError> {
        drift(self, dir, args, w).await.map_err(back_to_migrate)
    }

    async fn backends(&self) -> Result<Vec<Backend>, MigrateError> {
        fleet::backends(&self.pools.registry_pool())
            .await
            .map_err(back_to_migrate)
    }

    async fn prune_gate(
        &self,
        dir: &Path,
        id: &str,
    ) -> Result<crate::migrate::v2::squash::PruneGate, MigrateError> {
        fleet::prune_gate(self.pools, dir, self.registry_url, id)
            .await
            .map_err(back_to_migrate)
    }
}

/// `apply`'s flags under tenancy: the fleet's rollout flags and `--plan`.
const APPLY_USAGE: &str = "dbmigrate apply [--plan | --dry-run] [--max-failures N | --fail-fast] \
    [--parallel K] [--canary a,b] [--active-only | --include-inactive] [--phase expand] \
    [--allow-mixed-heads] [--abandon-fleet-run]   the registry chain, then every tenant";

/// Rollout flags (8.3), and whether `--plan` was given.
fn fleet_flags(args: &[String]) -> Result<(FleetOptions, bool), TenancyError> {
    let parsed = super::args::parse(
        args,
        &super::args::Spec {
            verb: "dbmigrate apply",
            usage: APPLY_USAGE,
            switches: &[
                "--plan",
                "--dry-run",
                "--fail-fast",
                "--active-only",
                "--include-inactive",
                "--allow-mixed-heads",
                "--abandon-fleet-run",
            ],
            valued: &["--max-failures", "--parallel", "--canary", "--phase"],
            max_positionals: 0,
        },
    )?;
    let number = |flag: &str| -> Result<Option<usize>, TenancyError> {
        parsed
            .value(flag)?
            .map(|v| {
                v.parse::<usize>()
                    .map_err(|_| TenancyError::Validation(format!("{flag} {v}: a number")))
            })
            .transpose()
    };
    if parsed.has("--active-only") && parsed.has("--include-inactive") {
        return Err(TenancyError::Validation(
            "--active-only and --include-inactive: pick one".into(),
        ));
    }
    let expand_only = match parsed.value("--phase")? {
        None => false,
        Some("expand") => true,
        Some(v) => {
            return Err(TenancyError::Validation(format!(
                "--phase {v}: only `expand`; a plain run applies contract files"
            )))
        }
    };
    let max_failures = match (number("--max-failures")?, parsed.has("--fail-fast")) {
        (Some(_), true) => {
            return Err(TenancyError::Validation(
                "--max-failures and --fail-fast: pick one".into(),
            ))
        }
        (m, fast) => m.or(fast.then_some(0)),
    };
    let fo = FleetOptions {
        max_failures,
        parallel: number("--parallel")?.unwrap_or(1),
        canaries: parsed
            .value("--canary")?
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        active_only: parsed.has("--active-only"),
        expand_only,
        allow_mixed_heads: parsed.has("--allow-mixed-heads"),
        abandon: parsed.has("--abandon-fleet-run"),
        migrator: fleet::MigratorLogin::from_env(),
    };
    Ok((fo, parsed.has("--plan") || parsed.has("--dry-run")))
}

/// The registry chain, then every tenant's, and the report; with `--plan`
/// each one's statements and nothing written. Failed tenants are listed
/// with their error class; any failure exits non-zero.
async fn apply<DB: Database>(
    f: &FleetCli<'_, DB>,
    dir: &Path,
    args: &[String],
    w: Out<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (fo, plan) = fleet_flags(args)?;
    if plan {
        let mut refused = 0;
        for (label, planned) in fleet::plan_fleet(f.pools, dir, f.registry_url).await? {
            writeln!(w, "-- {label}")?;
            match planned {
                Ok(lines) if lines.is_empty() => writeln!(w, "nothing to apply")?,
                Ok(lines) => {
                    for l in lines {
                        writeln!(w, "{l}")?;
                    }
                }
                Err(e) => {
                    refused += 1;
                    writeln!(w, "refused: {e}")?;
                }
            }
        }
        if refused > 0 {
            return Err(TenancyError::Validation(format!(
                "{refused} database(s) would refuse; see the plan above"
            )));
        }
        return Ok(());
    }
    let report = fleet::run_fleet(f.pools, dir, f.registry_url, &fo).await?;
    writeln!(
        w,
        "registry: applied {} migration(s)",
        report.registry.len()
    )?;
    for id in &report.registry {
        writeln!(w, "  + {id}")?;
    }
    for t in &report.tenants {
        match &t.error_class {
            Some(class) => writeln!(w, "tenant {}: failed ({class})", t.slug)?,
            None => writeln!(
                w,
                "tenant {}: applied {} migration(s)",
                t.slug,
                t.applied.len()
            )?,
        }
    }
    if report.stopped {
        writeln!(
            w,
            "stopped: a canary failed or the failure limit was passed"
        )?;
    }
    let failed = report.failed().count();
    if failed > 0 {
        return Err(TenancyError::Validation(format!(
            "{failed} of {} tenant(s) failed; see the report above",
            report.tenants.len()
        )));
    }
    Ok(())
}

const DOWN: crate::cli_args::Spec<'static> = crate::cli_args::Spec {
    verb: "dbmigrate down",
    usage: "dbmigrate down [<n> | --to <id>] (--registry | --tenant <slug> | --all-tenants) \
            [--plan | --dry-run] [--accept-data-loss] [--cascade]   tenants roll back before \
            the registry",
    switches: &[
        "--registry",
        "--all-tenants",
        "--plan",
        "--dry-run",
        "--accept-data-loss",
        "--cascade",
    ],
    valued: &["--to", "--tenant"],
    max_positionals: 1,
};

/// `down` under tenancy (8.4): the chosen tenants first, then with
/// `--registry` the registry, under the fleet lock. The first failure
/// stops it, so the registry never rolls back past a tenant.
async fn down<DB: Database>(
    f: &FleetCli<'_, DB>,
    dir: &Path,
    args: &[String],
    w: Out<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let p = crate::cli_args::parse(args, &DOWN)?;
    let back = Back::of(&p).map_err(TenancyError::Migrate)?;
    let tenants = match (p.value("--tenant")?, p.has("--all-tenants")) {
        (Some(_), true) => {
            return Err(TenancyError::Validation(
                "--tenant and --all-tenants: pick one".into(),
            ))
        }
        (Some(slug), false) => Some(Tenants::One(slug)),
        (None, true) => Some(Tenants::All),
        (None, false) => None,
    };
    let registry_too = p.has("--registry");
    if tenants.is_none() && !registry_too {
        return Err(TenancyError::Validation(format!(
            "under tenancy `down` needs --registry, --tenant <slug> or --all-tenants — usage: {}",
            DOWN.usage
        )));
    }
    let registry = f.pools.registry_pool();
    let (reg, back) = (&registry, &back);
    let body =
        move || async move { Ok(down_locked(f, dir, back, tenants, registry_too, reg, w).await) };
    run::fleet_locked(reg, body).await?
}

async fn down_locked<DB: Database>(
    f: &FleetCli<'_, DB>,
    dir: &Path,
    back: &Back<'_>,
    tenants: Option<Tenants<'_>>,
    registry_too: bool,
    registry: &crate::sql::Pool,
    w: Out<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let dry = back.dry_run();
    if let Some(which) = tenants {
        for (slug, t) in fleet::targets(f.pools, f.registry_url, which).await? {
            let stop = |e: &dyn std::fmt::Display| {
                TenancyError::Validation(format!(
                    "tenant `{slug}`: {e}; later tenants and the registry were not rolled back"
                ))
            };
            let t = t.map_err(|e| stop(&e))?;
            let open = t.open().await.map_err(|e| stop(&e))?;
            let r = back
                .run(&open.pool, dir, &back.opts(fleet::tenant_opts(&t)))
                .await;
            let recorded = match (&r, dry) {
                (Ok(_), false) => fleet::record_head(registry, &t, &open.pool).await,
                _ => Ok(()),
            };
            open.close().await;
            let report = r.map_err(|e| stop(&e))?;
            recorded?;
            crate::migrate::v2::cli::write_back(w, Some(&format!("tenant {slug}")), &report, dry)?;
        }
    }
    if registry_too {
        let opts = back.opts(Options {
            run_as: RunAs::Registry,
            ..Options::default()
        });
        let report = back.run(registry, dir, &opts).await?;
        crate::migrate::v2::cli::write_back(w, Some("registry"), &report, dry)?;
    }
    Ok(())
}

/// `status` for the registry, then each tenant.
async fn status<DB: Database>(
    f: &FleetCli<'_, DB>,
    dir: &Path,
    args: &[String],
    w: Out<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    super::args::no_args(args, "dbmigrate status")?;
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| TenancyError::Migrate(MigrateError::Validation(e.to_string())))?;
    let registry = f.pools.registry_pool();
    let reg = Options {
        run_as: RunAs::Registry,
        ..Options::default()
    };
    writeln!(w, "registry:")?;
    for l in crate::migrate::v2::cli::status_lines(&registry, &project, &reg).await? {
        writeln!(w, "  {l}")?;
    }
    for (slug, t) in fleet::targets(f.pools, f.registry_url, Tenants::All).await? {
        writeln!(w, "tenant {slug}:")?;
        let lines = match t {
            Err(e) => Err(e.to_string()),
            Ok(t) => match t.open().await {
                Err(e) => Err(e.to_string()),
                Ok(open) => {
                    let opts = fleet::tenant_opts(&t);
                    let r =
                        crate::migrate::v2::cli::status_lines(&open.pool, &project, &opts).await;
                    open.close().await;
                    r.map_err(|e| e.to_string())
                }
            },
        };
        match lines {
            Ok(lines) => {
                for l in lines {
                    writeln!(w, "  {l}")?;
                }
            }
            Err(e) => writeln!(w, "  unreachable: {e}")?,
        }
    }
    Ok(())
}

/// `check --drift` for the registry, then each tenant; any difference, or
/// a tenant that cannot be read, fails.
async fn drift<DB: Database>(
    f: &FleetCli<'_, DB>,
    dir: &Path,
    args: &[String],
    w: Out<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let rest: Vec<String> = args.iter().filter(|a| *a != "--drift").cloned().collect();
    super::args::no_args(&rest, "dbmigrate check --drift")?;
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| TenancyError::Migrate(MigrateError::Validation(e.to_string())))?;
    let mut found = 0usize;
    let mut each = |w: &mut (dyn Write + Send),
                    label: &str,
                    r: Result<Vec<String>, String>|
     -> Result<(), TenancyError> {
        match r {
            Ok(diff) if diff.is_empty() => writeln!(w, "{label}: No drift.")?,
            Ok(diff) => {
                found += diff.len();
                for d in diff {
                    writeln!(w, "{label}: {d}")?;
                }
            }
            Err(e) => {
                found += 1;
                writeln!(w, "{label}: unreadable: {e}")?;
            }
        }
        Ok(())
    };
    let registry = f.pools.registry_pool();
    let reg = crate::migrate::v2::cli::drift_lines(&registry, &project, Target::Registry)
        .await
        .map_err(|e| e.to_string());
    each(w, "registry", reg)?;
    for (slug, t) in fleet::targets(f.pools, f.registry_url, Tenants::All).await? {
        let r = match t {
            Err(e) => Err(e.to_string()),
            Ok(t) => match t.open().await {
                Err(e) => Err(e.to_string()),
                Ok(open) => {
                    let r =
                        crate::migrate::v2::cli::drift_lines(&open.pool, &project, Target::Tenant)
                            .await;
                    open.close().await;
                    r.map_err(|e| e.to_string())
                }
            },
        };
        each(w, &format!("tenant {slug}"), r)?;
    }
    if found > 0 {
        return Err(TenancyError::Validation(format!(
            "dbmigrate check --drift: {found} difference(s) from the migrations' state"
        )));
    }
    Ok(())
}
