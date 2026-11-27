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
use crate::tenancy::migrate_v2::{self as fleet, FleetOptions, Reached, Tenants};
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
        ..FleetOptions::default()
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
    let (mut fo, plan) = fleet_flags(args)?;
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
    // SIGTERM or Ctrl-C: each tenant finishes its unit in flight, then it stops (6.11).
    let on_signal = crate::shutdown::StopOnSignal::listen();
    fo.stop = Some(on_signal.flag());
    let report = fleet::run_fleet(f.pools, dir, f.registry_url, &fo).await?;
    drop(on_signal);
    writeln!(
        w,
        "registry: applied {} migration(s)",
        report.registry.len()
    )?;
    for id in &report.registry {
        writeln!(w, "  + {id}")?;
    }
    for t in &report.tenants {
        match (&t.error_class, &t.error) {
            (Some(class), Some(e)) => writeln!(w, "tenant {}: failed ({class}): {e}", t.slug)?,
            (Some(class), None) => writeln!(w, "tenant {}: failed ({class})", t.slug)?,
            (None, _) => writeln!(
                w,
                "tenant {}: applied {} migration(s)",
                t.slug,
                t.applied.len()
            )?,
        }
    }
    for slug in &report.skipped {
        writeln!(w, "tenant {slug}: skipped, not run")?;
    }
    if report.stopped {
        writeln!(
            w,
            "stopped: a canary failed or the failure limit was passed"
        )?;
    }
    if let Some(e) = report.stop_error() {
        return Err(e);
    }
    let failed = report.failed().count();
    if failed > 0 {
        return Err(TenancyError::Validation(format!(
            "{failed} of {} tenant(s) failed, {} skipped; see the report above",
            report.tenants.len() + report.skipped.len(),
            report.skipped.len()
        )));
    }
    Ok(())
}

const DOWN: crate::cli_args::Spec<'static> = crate::cli_args::Spec {
    verb: "dbmigrate down",
    usage: "dbmigrate down [<n> | --to <id>] (--registry | --tenant <slug> | --all-tenants) \
            [--plan | --dry-run] [--accept-data-loss] [--cascade] [--abandon-fleet-run]   \
            tenants roll back before the registry",
    switches: &[
        "--registry",
        "--all-tenants",
        "--plan",
        "--dry-run",
        "--accept-data-loss",
        "--cascade",
        "--abandon-fleet-run",
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
    let cmd = DownCmd {
        back: &back,
        tenants,
        registry_too,
        abandon: p.has("--abandon-fleet-run"),
    };
    let (reg, cmd) = (&registry, &cmd);
    // Boxed: inline, the down future overflowed a debug test thread's stack.
    let body = move || async move { Ok(Box::pin(down_locked(f, dir, cmd, reg, w)).await) };
    run::fleet_locked(reg, body).await?
}

/// One `down` command under tenancy.
struct DownCmd<'a> {
    back: &'a Back<'a>,
    tenants: Option<Tenants<'a>>,
    registry_too: bool,
    abandon: bool,
}

impl DownCmd<'_> {
    /// The command as its fleet record names it: a rerun of the same one
    /// resumes that record.
    fn spec(&self) -> String {
        let how = match self.back.to() {
            Some(to) => format!("--to {to}"),
            None => self.back.steps().to_string(),
        };
        let scope = match &self.tenants {
            Some(Tenants::All) => " --all-tenants".to_owned(),
            Some(Tenants::One(slug)) => format!(" --tenant {slug}"),
            None => String::new(),
        };
        let reg = if self.registry_too { " --registry" } else { "" };
        format!("down {how}{scope}{reg}")
    }
}

/// A `down` run's record detail: the command, and for a count of steps the
/// head each database keeps (8.4), resolved once.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct DownDetail {
    spec: String,
    /// Slug, or [`REGISTRY`], to the newest `app:id` it keeps (`None`: none).
    heads: std::collections::BTreeMap<String, Option<String>>,
}

/// The registry's key in [`DownDetail::heads`]; no slug is empty.
const REGISTRY: &str = "";

async fn down_locked<DB: Database>(
    f: &FleetCli<'_, DB>,
    dir: &Path,
    cmd: &DownCmd<'_>,
    registry: &crate::sql::Pool,
    w: Out<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let spec = cmd.spec();
    let reg_opts = || {
        cmd.back.opts(Options {
            run_as: RunAs::Registry,
            ..Options::default()
        })
    };
    // A `down N` rerun while that run is open goes to the same heads.
    let open = crate::migrate::v2::ledger::fleet_open_down(registry).await?;
    let resumed = match &open {
        Some((id, detail)) => {
            let d: DownDetail = detail
                .as_deref()
                .and_then(|d| serde_json::from_str(d).ok())
                .unwrap_or_default();
            if d.spec == spec {
                Some((id.clone(), d))
            } else if cmd.back.to().is_some() || cmd.abandon {
                None
            } else {
                return Err(TenancyError::Validation(format!(
                    "fleet run {id} (`{}`) did not finish: rerun that command to finish it, roll \
                     back with `--to <id>`, or pass --abandon-fleet-run",
                    d.spec
                )));
            }
        }
        None => None,
    };
    let heads = match &resumed {
        Some((_, d)) => d.heads.clone(),
        None if cmd.back.to().is_some() => std::collections::BTreeMap::default(),
        None => resolve_heads(f, cmd, registry, &reg_opts()).await?,
    };
    if cmd.back.dry_run() {
        return Box::pin(down_each(f, dir, cmd, &heads, registry, w)).await;
    }
    let run_id = if let Some((id, _)) = resumed {
        crate::migrate::v2::ledger::fleet_reopen(registry, &id).await?;
        id
    } else {
        if let Some((id, _)) = &open {
            crate::migrate::v2::ledger::fleet_abandon(registry, id).await?;
        }
        let project = crate::migrate::v2::load::load_project(dir)
            .map_err(|e| TenancyError::Migrate(MigrateError::Validation(e.to_string())))?;
        let id = run::new_run_id();
        let detail = DownDetail {
            spec,
            heads: heads.clone(),
        };
        crate::migrate::v2::ledger::fleet_open(
            registry,
            &id,
            "down",
            &run::plan_hash(&project),
            cmd.abandon,
            Some(&serde_json::to_string(&detail).map_err(MigrateError::from)?),
        )
        .await?;
        id
    };
    let result = fleet::beating(
        registry,
        &run_id,
        None,
        Box::pin(down_each(f, dir, cmd, &heads, registry, w)),
    )
    .await;
    let state = if result.is_ok() {
        "succeeded"
    } else {
        "failed"
    };
    crate::migrate::v2::ledger::fleet_close(registry, &run_id, state).await?;
    result
}

/// For a count of steps, the head each chosen database keeps, read once
/// before anything rolls back.
async fn resolve_heads<DB: Database>(
    f: &FleetCli<'_, DB>,
    cmd: &DownCmd<'_>,
    registry: &crate::sql::Pool,
    reg_opts: &Options,
) -> Result<std::collections::BTreeMap<String, Option<String>>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let steps = cmd.back.steps();
    let mut heads = std::collections::BTreeMap::new();
    if let Some(which) = &cmd.tenants {
        for (slug, t) in fleet::targets(f.pools, f.registry_url, *which).await? {
            let t = match t {
                Reached::Target(t) => t,
                Reached::MissingSchema => continue,
                Reached::Unreachable(e) => {
                    return Err(TenancyError::Validation(format!(
                        "tenant `{slug}`: {e}; nothing was rolled back"
                    )))
                }
            };
            let open = t.open().await?;
            let head = run::head_after_down(&open.pool, &fleet::tenant_opts(&t), steps).await;
            open.close().await;
            heads.insert(slug, head.map_err(|e| TenancyError::Migrate(e.into()))?);
        }
    }
    if cmd.registry_too {
        let head = run::head_after_down(registry, reg_opts, steps)
            .await
            .map_err(|e| TenancyError::Migrate(e.into()))?;
        heads.insert(REGISTRY.to_owned(), head);
    }
    Ok(heads)
}

/// Roll each chosen tenant back, then the registry: to `--to`, or to its
/// head in `heads` (one left out was not part of the run).
async fn down_each<DB: Database>(
    f: &FleetCli<'_, DB>,
    dir: &Path,
    cmd: &DownCmd<'_>,
    heads: &std::collections::BTreeMap<String, Option<String>>,
    registry: &crate::sql::Pool,
    w: Out<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let back = cmd.back;
    let dry = back.dry_run();
    // How many steps reach `key`'s head on `pool`; `None` when it is not in the run.
    let steps_on = |key: &str, pool: &crate::sql::Pool, opts: &Options| {
        let head = heads.get(key).cloned();
        let (pool, opts) = (pool.clone(), opts.clone());
        async move {
            if back.to().is_some() {
                return Ok(Some(0));
            }
            let Some(head) = head else { return Ok(None) };
            run::steps_to_head(&pool, &opts, head.as_deref())
                .await
                .map(Some)
                .map_err(|e| TenancyError::Migrate(e.into()))
        }
    };
    if let Some(which) = &cmd.tenants {
        for (slug, t) in fleet::targets(f.pools, f.registry_url, *which).await? {
            let stop = |e: &dyn std::fmt::Display| {
                TenancyError::Validation(format!(
                    "tenant `{slug}`: {e}; later tenants and the registry were not rolled back: \
                     rerun the same command to finish (it stops at the same targets), or use \
                     `--to <id>`"
                ))
            };
            let t = match t {
                Reached::Target(t) => t,
                Reached::MissingSchema => {
                    writeln!(w, "tenant {slug}: schema missing, nothing to roll back")?;
                    continue;
                }
                Reached::Unreachable(e) => return Err(stop(&e)),
            };
            let open = t.open().await.map_err(|e| stop(&e))?;
            let opts = back.opts(fleet::tenant_opts(&t));
            let r = match steps_on(&slug, &open.pool, &opts).await {
                Ok(None) => Ok(None),
                Ok(Some(n)) if n == 0 && back.to().is_none() => Ok(Some(run::Report::default())),
                Ok(Some(n)) => back
                    .run_steps(&open.pool, dir, &opts, n)
                    .await
                    .map(Some)
                    .map_err(TenancyError::Migrate),
                Err(e) => Err(e),
            };
            let recorded = match (&r, dry) {
                (Ok(Some(_)), false) => fleet::record_head(registry, &t, &open.pool).await,
                _ => Ok(()),
            };
            open.close().await;
            let Some(report) = r.map_err(|e| stop(&e))? else {
                continue;
            };
            recorded?;
            crate::migrate::v2::cli::write_back(w, Some(&format!("tenant {slug}")), &report, dry)?;
        }
    }
    if cmd.registry_too {
        let opts = back.opts(Options {
            run_as: RunAs::Registry,
            ..Options::default()
        });
        let report = match steps_on(REGISTRY, registry, &opts).await? {
            Some(0) if back.to().is_none() => run::Report::default(),
            Some(n) => back.run_steps(registry, dir, &opts, n).await?,
            None => back.run(registry, dir, &opts).await?,
        };
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
            Reached::Unreachable(e) => Err(e.to_string()),
            Reached::MissingSchema => Ok(vec!["schema missing".to_owned()]),
            Reached::Target(t) => match t.open().await {
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
            Reached::Unreachable(e) => Err(e.to_string()),
            Reached::MissingSchema => Ok(vec!["schema missing".to_owned()]),
            Reached::Target(t) => match t.open().await {
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
