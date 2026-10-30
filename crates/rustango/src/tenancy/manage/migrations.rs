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
    migrate_tenants_in(pools, registry_url, dir, w).await
}

/// Tenant-scoped files of `dir`'s project across active tenants, then the report.
async fn migrate_tenants_in<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
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

/// Registry-scoped files of `dir`'s project, then what was applied.
async fn migrate_registry_in<W: Write, DB: Database>(
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

/// Every tenant's names in each of `ledgers`, inactive tenants included,
/// opening each tenant pool once. A tenant that can't be read fails the
/// whole call, so nothing is assumed.
pub(super) async fn tenant_ledgers<DB: Database, const N: usize>(
    pools: &TenantPools<DB>,
    ledgers: [&str; N],
) -> Result<[TenantLedger; N], rustango::migrate::MigrateError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::sql::FetcherPool as _;
    use rustango::migrate::MigrateError;

    let unreadable = |slug: &str, e: &dyn std::fmt::Display| {
        MigrateError::Validation(format!(
            "could not read tenant `{slug}`'s migration ledger: {e}"
        ))
    };
    let orgs: Vec<crate::tenancy::org::Org> = crate::tenancy::org::Org::objects()
        .fetch(&pools.registry_pool())
        .await?;
    let mut out: [TenantLedger; N] = std::array::from_fn(|_| Vec::with_capacity(orgs.len()));
    for org in orgs {
        let read = org_ledgers(pools, &org, ledgers)
            .await
            .map_err(|e| unreadable(&org.slug, &e))?;
        for (applied, out) in read.into_iter().zip(&mut out) {
            out.push((org.slug.clone(), applied));
        }
    }
    Ok(out)
}

/// `org`'s names in each of `ledgers`, through a pool closed or uncached after.
async fn org_ledgers<DB: Database, const N: usize>(
    pools: &TenantPools<DB>,
    org: &crate::tenancy::org::Org,
    ledgers: [&str; N],
) -> Result<[std::collections::HashSet<String>; N], String>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (pool, own) = pools
        .existing_pool_dyn(org)
        .await
        .map_err(|e| e.to_string())?;
    let mut out: [std::collections::HashSet<String>; N] =
        std::array::from_fn(|_| Default::default());
    let mut read = Ok(());
    for (ledger, out) in ledgers.iter().zip(&mut out) {
        match rustango::migrate::applied_or_empty(&pool, ledger).await {
            Ok(applied) => *out = applied,
            Err(e) => {
                read = Err(e.to_string());
                break;
            }
        }
    }
    if own {
        pool.close().await;
    } else {
        // A cached pool per tenant would stay open for the whole command.
        pools.invalidate(&org.slug).await;
    }
    read.map(|()| out)
}

/// Tenant ledgers `check` reads at once.
const CHECK_CONCURRENCY: usize = 8;
/// A tenant whose ledger takes longer is reported as unreadable, so an
/// unreachable database costs this, not the pool's acquire timeout.
const CHECK_TENANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Every active tenant's project and system ledgers, or why they could not be
/// read: what `check` compares the tenant-scoped files with. One bad tenant spoils no other.
pub(super) async fn active_tenant_ledgers<DB: Database>(
    pools: &TenantPools<DB>,
) -> Result<rustango::migrate::manage::TenantApplied, String>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    use crate::tenancy::org::Org;
    use futures_util::StreamExt as _;
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&pools.registry_pool())
        .await
        .map_err(|e| e.to_string())?;
    let mut out: rustango::migrate::manage::TenantApplied = futures_util::stream::iter(orgs)
        .map(|org| async move {
            use rustango::migrate::{LEDGER_TABLE, SYSTEM_LEDGER_TABLE};
            let read = org_ledgers(pools, &org, [LEDGER_TABLE, SYSTEM_LEDGER_TABLE]);
            let read = tokio::time::timeout(CHECK_TENANT_TIMEOUT, read)
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "timed out after {}s",
                        CHECK_TENANT_TIMEOUT.as_secs()
                    ))
                });
            let read = read.map(|[project, system]| rustango::migrate::manage::TenantNames {
                project,
                system,
            });
            (org.slug, read)
        })
        .buffer_unordered(CHECK_CONCURRENCY)
        .collect()
        .await;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// `(slug, applied names)` per tenant, for one ledger.
pub(super) type TenantLedger = Vec<(String, std::collections::HashSet<String>)>;

/// `showmigrations`: registry-scoped files from the registry ledger,
/// tenant-scoped ones, framework steps too, from every tenant's.
pub(super) async fn showmigrations_cmd<W: Write, DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use rustango::migrate::{LEDGER_TABLE, SYSTEM_LEDGER_TABLE};
    let [project, system] = tenant_ledgers(pools, [LEDGER_TABLE, SYSTEM_LEDGER_TABLE])
        .await
        .map_err(TenancyError::Migrate)?;
    rustango::migrate::manage::showmigrations_with(
        &pools.registry_pool(),
        dir,
        &rustango::migrate::manage::SystemView::registry(dir),
        Some(&rustango::migrate::manage::TenantChains {
            project: &project,
            system: &system,
        }),
        w,
    )
    .await
    .map_err(TenancyError::Migrate)
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
    migrate_registry_in(pools, dir, w).await
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
             migrate <target>                forward or back to <target> (registry-scoped only; tenants: migrate-tenants\n\
                                             forward, downgrade --tenant <slug> | --all-tenants back)\n\
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
    if fakes.is_empty() && (fake_scope == FakeScope::System || fake_all_tenants) {
        return Err(TenancyError::Migrate(
            rustango::migrate::MigrateError::Validation(
                "--system and --all-tenants only apply to `--fake <name>`".into(),
            ),
        ));
    }
    if !fakes.is_empty() {
        let (fake_dir, ledger) = fake_scope.resolve(dir);
        check_fake_names(&fake_dir, &fakes, fake_all_tenants)?;
        if fake_all_tenants {
            return fake_apply_across_tenants(pools, ledger, &fakes, w).await;
        }
        if fake_scope == FakeScope::System {
            return fake_apply_to_pool(
                &pools.registry_pool(),
                ledger,
                &fakes,
                "registry (system chain)",
                w,
            )
            .await;
        }
        return fake_apply_to_registry(pools, &fakes, w).await;
    }
    if target.is_some() || dry_run {
        // Targeted / dry-run mode is registry-only — tenant-scoped
        // routing for arbitrary targets isn't well-defined yet.
        return run_registry_scoped(pools, dir, args, w).await;
    }

    // As boot does: the registry, then the tenants through the one
    // dispatch seam (see `migrate_tenants_dyn_with_progress`); each walks
    // the app folders itself.
    migrate_registry_in(pools, dir, w).await?;
    migrate_tenants_in(pools, registry_url, dir, w).await
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
    let target = args.iter().find(|a| !a.starts_with('-'));
    if target.is_none() && args.iter().any(|a| a == "--dry-run") {
        // The registry's system chain is registry-scoped; the app folders
        // are previewed as `migrate` applies them.
        let project = tenant_migrate::ScopedProject::of(dir, MigrationScope::Registry).await?;
        let system = rustango::migrate::manage::SystemView::registry(dir);
        return rustango::migrate::manage::migrate_dry_run_cmd(
            &pools.registry_pool(),
            project.dirs(),
            &system,
            w,
        )
        .await
        .map_err(TenancyError::Migrate);
    }
    // The folder holding the target: the project's own, or an app's.
    let home = match target {
        Some(target) => target_home(dir, target)?,
        None => None,
    };
    if let (Some(target), Some((_, MigrationScope::Tenant))) = (target, &home) {
        return Err(TenancyError::Validation(format!(
            "`{target}` is tenant-scoped — `migrate <target>` moves the registry only; \
             apply it with `migrate-tenants`, roll it back with \
             `downgrade [N] --tenant <slug>` or `downgrade [N] --all-tenants`"
        )));
    }
    // Forward: the tenancy runner, with the project's registry-scope system
    // chain. The single-database path would build one from the copy.
    let dry_run = args.iter().any(|a| a == "--dry-run");
    if let Some(target) = target.filter(|t| *t != "zero" && !dry_run) {
        let registry = pools.registry_pool();
        let applied =
            rustango::migrate::applied_or_empty(&registry, rustango::migrate::LEDGER_TABLE).await?;
        // `None` too when applied meanwhile: the move is then backward.
        if !applied.contains(target.as_str()) {
            let moved =
                tenant_migrate::migrate_registry_to(&pools.registry_pool(), dir, target).await?;
            if let Some(moved) = moved {
                for m in &moved.unapplied {
                    writeln!(w, "  unapplied {}", m.name)?;
                }
                for m in &moved.system {
                    writeln!(w, "  applied {} (system)", m.name)?;
                }
                for m in &moved.applied {
                    writeln!(w, "  applied {}", m.name)?;
                }
                return Ok(());
            }
        }
    }
    let source = home.map_or_else(|| dir.to_path_buf(), |(d, _)| d);
    // An app folder is always copied: run in place, it would get a `system/`.
    let scoped = if source.as_path() == dir {
        tenant_migrate::scoped_subset(dir, MigrationScope::Registry).await?
    } else {
        tenant_migrate::ScopedDir::Owned(tenant_migrate::scoped_copy(
            &source,
            MigrationScope::Registry,
        )?)
    };
    let mut forwarded = vec!["migrate".to_owned()];
    forwarded.extend(args.iter().cloned());
    rustango::migrate::manage::run_with_writer(
        &pools.registry_pool(),
        scoped.path(&source),
        forwarded,
        w,
    )
    .await
    .map_err(TenancyError::Migrate)
}

/// The one folder of `dir`'s project holding `target`, and its scope. The
/// ledger keys by name, so a name in two folders is refused.
fn target_home(
    dir: &Path,
    target: &str,
) -> Result<Option<(std::path::PathBuf, rustango::migrate::MigrationScope)>, TenancyError> {
    let mut found: Vec<(std::path::PathBuf, rustango::migrate::MigrationScope)> = Vec::new();
    for d in rustango::migrate::file::project_migration_dirs(dir) {
        let all = rustango::migrate::file::list_dir(&d)?;
        if let Some(m) = all.iter().find(|m| m.name == target) {
            found.push((d, m.scope));
        }
    }
    if let [(a, _), (b, _), ..] = found.as_slice() {
        return Err(TenancyError::Validation(format!(
            "`{target}` is in both {} and {}; rename one, so the target is unambiguous",
            a.display(),
            b.display()
        )));
    }
    Ok(found.pop())
}

// ---------- downgrade (scope-aware) ----------

const DOWNGRADE_USAGE: &str =
    "downgrade [N | <target>] [--tenant <slug> | --all-tenants [--yes]]   roll back the last N \
     (default 1) applied migrations, or those of <target>'s folder applied after it: \
     registry-scoped, or tenant-scoped in one tenant / every active tenant. --all-tenants \
     prints each tenant's plan; --yes runs it";

/// `downgrade [N]` on the registry ledger, or with `--tenant <slug>` /
/// `--all-tenants` on the tenant ledgers, over that scope's files only.
/// Tenants can sit at different versions, so `--all-tenants` shows each
/// one's plan and runs it only with `--yes`.
pub(super) async fn downgrade_cmd<W: Write, DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Column as _;
    use crate::migrate::Rollback;
    use crate::sql::FetcherPool as _;
    use crate::tenancy::org::Org;
    use rustango::migrate::MigrationScope;

    let parsed = super::args::parse(
        args,
        &super::args::Spec {
            verb: "downgrade",
            usage: DOWNGRADE_USAGE,
            switches: &["--all-tenants", "--yes"],
            valued: &["--tenant"],
            max_positionals: 1,
        },
    )?;
    let rollback = match parsed.positional(0) {
        Some(n) if n.chars().all(|c| c.is_ascii_digit()) => Rollback::Steps(
            n.parse::<usize>()
                .map_err(|_| TenancyError::Validation(format!("invalid step count: {n}")))?,
        ),
        Some(target) => Rollback::After(target.as_str()),
        None => Rollback::Steps(1),
    };
    let slug = parsed.value("--tenant")?;
    let all = parsed.has("--all-tenants");
    if parsed.has("--yes") && !all {
        return Err(TenancyError::Validation(
            "--yes only applies to `--all-tenants`".into(),
        ));
    }
    let orgs: Vec<Org> = match (slug, all) {
        (Some(_), true) => {
            return Err(TenancyError::Validation(
                "--tenant and --all-tenants contradict each other — pass one".into(),
            ))
        }
        (None, false) => {
            let Rollback::Steps(_) = rollback else {
                return Err(TenancyError::Validation(
                    "a registry rollback to a target is `migrate <target>`; \
                     `downgrade <target>` needs `--tenant <slug>` or `--all-tenants`"
                        .into(),
                ));
            };
            let project = tenant_migrate::ScopedProject::of(dir, MigrationScope::Registry).await?;
            let ledger = rustango::migrate::LEDGER_TABLE;
            let touched = crate::migrate::rollback_pool(
                &pools.registry_pool(),
                project.dirs(),
                rollback,
                ledger,
            )
            .await?;
            for (_, m) in &touched.unapply {
                writeln!(w, "registry: rolled back {}", m.name)?;
            }
            if touched.unapply.is_empty() {
                writeln!(w, "registry: nothing to downgrade")?;
                let sources = rustango::migrate::file::project_migration_dirs(dir);
                let all_files = rustango::migrate::file::list_each(&sources)?;
                if all_files.iter().any(|m| m.scope == MigrationScope::Tenant) {
                    writeln!(
                        w,
                        "tenant-scoped migrations roll back with `downgrade [N] --tenant <slug>` \
                         or `downgrade [N] --all-tenants`"
                    )?;
                }
            }
            return Ok(());
        }
        (Some(slug), false) => {
            let found: Vec<Org> = Org::objects()
                .where_(Org::slug.eq(slug.to_owned()))
                .fetch(&pools.registry_pool())
                .await?;
            if found.is_empty() {
                return Err(TenancyError::Validation(format!(
                    "tenant `{slug}` not found"
                )));
            }
            found
        }
        (None, true) => {
            Org::objects()
                .where_(Org::active.eq(true))
                .fetch(&pools.registry_pool())
                .await?
        }
    };
    if orgs.is_empty() {
        writeln!(w, "no active tenants")?;
        return Ok(());
    }
    if let Rollback::After(target) = rollback {
        if !matches!(target_home(dir, target)?, Some((_, MigrationScope::Tenant))) {
            return Err(TenancyError::Validation(format!(
                "`{target}` is not a tenant-scoped migration"
            )));
        }
    }
    let project = tenant_migrate::ScopedProject::of(dir, MigrationScope::Tenant).await?;
    let run = !all || parsed.has("--yes");
    let mut failed = 0;
    for org in &orgs {
        let step = async {
            let (pool, own) = pools.existing_pool_dyn(org).await?;
            let result = async {
                let ledger = rustango::migrate::LEDGER_TABLE;
                let order = crate::migrate::ApplyOrder::read_or_empty(&pool, ledger).await?;
                let plan = rollback.plan(project.dirs(), &order)?;
                // Chosen again under the lock: a migrate in between moves the plan.
                let done = if !run || plan.unapply.is_empty() {
                    plan
                } else {
                    crate::migrate::rollback_pool(&pool, project.dirs(), rollback, ledger).await?
                };
                let names = done.unapply.into_iter().map(|(_, m)| m.name).collect();
                Ok::<(Vec<String>, Vec<String>), rustango::migrate::MigrateError>((
                    names, done.kept,
                ))
            }
            .await;
            if own {
                pool.close().await;
            }
            result.map_err(TenancyError::Migrate)
        };
        let result = step.await;
        match &result {
            Ok((names, _)) if names.is_empty() => {
                writeln!(w, "  · {}: nothing to downgrade", org.slug)?;
            }
            Ok((names, _)) if run => {
                writeln!(w, "  ✓ {}: rolled back {}", org.slug, names.join(", "))?;
            }
            Ok((names, _)) => {
                writeln!(w, "  · {}: would roll back {}", org.slug, names.join(", "))?;
            }
            Err(e) => {
                failed += 1;
                writeln!(w, "  ✗ {}: {e}", org.slug)?;
            }
        }
        if let (Ok((_, kept)), Rollback::After(target)) = (&result, rollback) {
            if !kept.is_empty() {
                writeln!(
                    w,
                    "    left applied (other folders, applied after {target}): {}",
                    kept.join(", ")
                )?;
            }
        }
    }
    if !run {
        writeln!(
            w,
            "dry run: nothing rolled back; add --yes to run this plan"
        )?;
    }
    tenant_failures(failed, orgs.len())
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
    names: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    fake_apply_to_pool(
        &registry,
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
        if let Err(e) = fake_apply_to_pool(&pool, ledger, names, &org.slug, w).await {
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

/// Each name must be a migration in `dir` of the scope being stamped:
/// tenant-scoped with `--all-tenants`, registry-scoped without.
fn check_fake_names(dir: &Path, names: &[String], all_tenants: bool) -> Result<(), TenancyError> {
    use rustango::migrate::MigrationScope;
    let refuse = |msg: String| {
        Err(TenancyError::Migrate(
            rustango::migrate::MigrateError::Validation(msg),
        ))
    };
    let want = if all_tenants {
        MigrationScope::Tenant
    } else {
        MigrationScope::Registry
    };
    let migrations = rustango::migrate::file::list_dir(dir).map_err(TenancyError::Migrate)?;
    for name in names {
        match migrations.iter().find(|m| &m.name == name) {
            None => {
                return refuse(format!(
                    "--fake: no migration named `{name}` in {} \
                     (run `showmigrations` to list available names)",
                    dir.display()
                ))
            }
            Some(m) if m.scope != want => {
                let (kind, fix) = if all_tenants {
                    ("registry", "drop `--all-tenants` to stamp the registry")
                } else {
                    ("tenant", "add `--all-tenants` to stamp every tenant")
                };
                return refuse(format!(
                    "--fake: `{name}` is a {kind}-scoped migration; {fix}"
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Backfill `ledger` in `pool` with `names` without running any SQL.
///
/// Recovery path for the "tables exist but the ledger row is missing" drift
/// that surfaces as `relation "X" already exists` (Postgres 42P07) /
/// `table already exists` (MySQL 1050) on the next `migrate` — a DB set up
/// out-of-band, a dropped ledger, a partially-succeeded migration, or a
/// subsystem whose tables predate its migration.
///
/// The caller checks each name with [`check_fake_names`] first. The
/// ledger is created if missing. `label` names
/// the target in the output (`registry`, a tenant slug, …).
async fn fake_apply_to_pool<W: Write>(
    pool: &crate::sql::Pool,
    ledger: &str,
    names: &[String],
    label: &str,
    w: &mut W,
) -> Result<(), TenancyError> {
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
