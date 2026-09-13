//! Tenant-lifecycle verbs: `create-tenant`, `drop-tenant`,
//! `purge-tenant`, `list-tenants`. Plus their parsers + the
//! database-mode admin-DROP helper.

use std::io::Write;
use std::path::Path;

use sqlx::Database;

use crate::core::Column as _;
use crate::sql::FetcherPool;

use crate::tenancy::error::TenancyError;
use crate::tenancy::manage::args::{next_value, reject_leading_flag};
use crate::tenancy::manage_interactive;
use crate::tenancy::org::{BackendKind, Org, StorageMode};
use crate::tenancy::pools::TenantPools;
use crate::tenancy::provision;

// ---------- create-tenant ----------

/// The verb: turn `argv` into a [`ProvisionRequest`], run the engine,
/// print what happens.
///
/// Everything that is not about a command line lives in
/// [`crate::tenancy::provision`] — so an HTTP handler, a webhook or a
/// job can stand up a tenant without faking an `argv` and handing it a
/// `Vec<u8>` to write into.
pub(super) async fn create_tenant<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let request = parse_create_tenant_args(args)?;

    // Steps print as they happen rather than from the outcome, because
    // "created tenant …" has always appeared *before* the migrations,
    // and the summary after them.
    let progress = CreateTenantProgress {
        out: std::sync::Mutex::new(w),
        slug: request.slug.clone(),
        mode: request.mode,
    };
    // Recorded, like the console's provisioning (#1344). A tenant created
    // from a shell used to leave no run at all, so the run history — and
    // `list-runs` — described only what the console had done, and a CLI
    // provision that died halfway left nothing to find. Recording is
    // best-effort inside `provision_tenant_recorded`: bookkeeping must not
    // be what fails a tenant creation.
    let (_run, outcome) = provision::provision_tenant_recorded(
        pools,
        registry_url,
        dir,
        &request,
        Some(&progress),
        Some("cli"),
        None,
    )
    .await?;

    let w = progress.into_inner();
    match &outcome.migrations {
        // The observer already printed the `--no-migrate` line; the
        // verb has never followed it with a summary.
        provision::MigrationsOutcome::Skipped => {}
        provision::MigrationsOutcome::Failed(err) => writeln!(w, "  migration failed: {err}")?,
        provision::MigrationsOutcome::Applied(applied) => {
            writeln!(w, "  applied {} migration(s)", applied.len())?;
            for m in applied {
                writeln!(w, "    + {}", m.name)?;
            }
        }
        provision::MigrationsOutcome::NotMatched => {
            writeln!(w, "  no migrations matched this tenant")?;
        }
    }
    Ok(())
}

/// Renders the engine's step events as the lines `create-tenant` has
/// always printed.
///
/// The `Mutex` bridges an observer's `&self` to the verb's `&mut W`; it
/// is uncontended, since the engine emits from one task. See
/// `CliProgress` in `manage::migrations` for the same reasoning about
/// why writing here does not violate the must-not-block contract.
struct CreateTenantProgress<'w, W> {
    out: std::sync::Mutex<&'w mut W>,
    slug: String,
    mode: StorageMode,
}

impl<'w, W: Write + Send> CreateTenantProgress<'w, W> {
    fn into_inner(self) -> &'w mut W {
        self.out.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W: Write + Send> provision::ProvisionObserver for CreateTenantProgress<'_, W> {
    fn on_event(&self, event: provision::ProvisionEvent) {
        use provision::{ProvisionEvent as E, ProvisionStep as S, StepStatus};

        let Ok(mut out) = self.out.lock() else {
            return;
        };
        // Progress is cosmetic; a broken pipe must not fail the verb.
        let _ = match event {
            E::Registered { org_id } => writeln!(
                out,
                "created tenant `{}` (id {org_id}, mode {})",
                self.slug, self.mode
            ),
            E::Step {
                step: S::Migrate,
                status: StepStatus::Started,
            } => writeln!(out, "  applying tenant migrations…"),
            E::Step {
                step: S::Migrate,
                status: StepStatus::Skipped(_),
            } => writeln!(out, "  --no-migrate: skipping tenant migrations"),
            // Validation and storage failures come back as `Err` from
            // the engine and are rendered by the caller; the remaining
            // steps are detail the CLI has never printed.
            _ => Ok(()),
        };
        let _ = out.flush();
    }
}

// ---------- test-tenant-connection ----------

/// Reach a candidate tenant database and report what is wrong with it,
/// without writing anything to the registry.
///
/// Takes no pools: the point is to answer "can I use this URL?" before
/// there is a tenant, so it is deliberately independent of the registry
/// the rest of the CLI is holding.
pub(super) async fn test_tenant_connection<W: Write + Send>(
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    const HELP: &str =
        "test-tenant-connection <database-url> [--no-write-probe] [--timeout <secs>]";
    reject_leading_flag(args, "test-tenant-connection", "database-url", HELP)?;

    let mut iter = args.iter();
    let url = iter.next().cloned().ok_or_else(|| {
        TenancyError::Validation(
            "test-tenant-connection requires a database URL positional argument".into(),
        )
    })?;

    let mut opts = crate::tenancy::preflight::Preflight::default();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--no-write-probe" => opts.probe_writes = false,
            "--timeout" => {
                let v = next_value(&mut iter, "--timeout")?;
                let secs: u64 = v.parse().map_err(|_| {
                    TenancyError::Validation(format!(
                        "--timeout must be a whole number of seconds, got `{v}`"
                    ))
                })?;
                opts.timeout = std::time::Duration::from_secs(secs);
            }
            "--help" | "-h" => return Err(TenancyError::Validation(HELP.to_owned())),
            other => {
                return Err(TenancyError::Validation(format!(
                    "test-tenant-connection: unknown argument `{other}`"
                )));
            }
        }
    }

    match crate::tenancy::preflight::check(&url, &opts).await {
        Ok(ok) => {
            writeln!(w, "ok: reached {}", ok.endpoint)?;
            if ok.writes_verified {
                writeln!(w, "  this role can create tables — migrations will run")?;
            } else {
                writeln!(
                    w,
                    "  --no-write-probe: did NOT check whether this role can create tables"
                )?;
            }
            Ok(())
        }
        // A `Validation` error rather than a driver one: nothing is
        // broken in rustango, the URL the operator supplied is wrong,
        // and the diagnosis already says what to change.
        Err(d) => Err(TenancyError::Validation(d.to_string())),
    }
}

/// `argv` → [`ProvisionRequest`]. The only place that knows
/// `--no-migrate` exists: a negative flag is right for a command line
/// and wrong for a struct field, so it is inverted on the way in.
fn parse_create_tenant_args(args: &[String]) -> Result<provision::ProvisionRequest, TenancyError> {
    const HELP: &str = "create-tenant <slug> [--mode schema|database] \
        [--backend postgres|mysql|sqlite] [--display-name <s>] \
        [--database-url <url>] [--schema-name <s>] [--host-pattern <s>] \
        [--port <n>] [--path-prefix <s>] [--no-migrate]";
    reject_leading_flag(args, "create-tenant", "slug", HELP)?;
    let mut iter = args.iter();
    let slug_arg = iter.next().cloned();
    let slug = match slug_arg {
        Some(s) => s,
        None => manage_interactive::ask("Tenant slug: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("create-tenant requires a slug positional argument".into())
            })?,
    };
    let mut out = provision::ProvisionRequest {
        slug,
        mode: StorageMode::Schema,
        backend: BackendKind::Postgres,
        display_name: None,
        database_url: None,
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        run_migrations: true,
        preflight: crate::tenancy::preflight::Preflight::default(),
    };
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--mode" => {
                let v = next_value(&mut iter, "--mode")?;
                out.mode = StorageMode::parse(&v).map_err(|got| {
                    TenancyError::Validation(format!(
                        "--mode must be `schema` or `database`, got `{got}`"
                    ))
                })?;
            }
            "--backend" => {
                let v = next_value(&mut iter, "--backend")?;
                out.backend = BackendKind::parse(&v).map_err(|got| {
                    TenancyError::Validation(format!(
                        "--backend must be `postgres`, `mysql`, or `sqlite`, got `{got}`"
                    ))
                })?;
            }
            "--display-name" => out.display_name = Some(next_value(&mut iter, "--display-name")?),
            "--database-url" => out.database_url = Some(next_value(&mut iter, "--database-url")?),
            "--schema-name" => out.schema_name = Some(next_value(&mut iter, "--schema-name")?),
            "--host-pattern" => out.host_pattern = Some(next_value(&mut iter, "--host-pattern")?),
            "--port" => {
                let v = next_value(&mut iter, "--port")?;
                out.port = Some(v.parse().map_err(|_| {
                    TenancyError::Validation(format!("--port must be an integer, got `{v}`"))
                })?);
            }
            "--path-prefix" => out.path_prefix = Some(next_value(&mut iter, "--path-prefix")?),
            "--no-migrate" => out.run_migrations = false,
            "--help" | "-h" => return Err(TenancyError::Validation(HELP.to_owned())),
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-tenant: unknown argument `{other}`"
                )));
            }
        }
    }
    // v0.33 — guard the unsupported pairs early so a clear error
    // surfaces instead of a generic schema-mode failure deep in the
    // pool layer.
    out.backend
        .validate_storage_mode(out.mode)
        .map_err(|msg| TenancyError::Validation(msg.to_owned()))?;
    Ok(out)
}

// ---------- drop-tenant ----------

pub(super) async fn drop_tenant<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(
        args,
        "drop-tenant",
        "slug",
        "drop-tenant <slug> [--confirm <slug>]\n  \
         Soft-delete: sets active=false. Data is preserved.\n  \
         `--confirm` must repeat the slug verbatim — interactive\n  \
         terminals can omit it and answer the prompt instead.",
    )?;
    let mut iter = args.iter();
    let slug_arg = iter.next().cloned();
    let mut confirm: Option<String> = None;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--confirm" => {
                confirm = Some(next_value(&mut iter, "--confirm")?);
            }
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "drop-tenant <slug> [--confirm <slug>]\n  \
                     Soft-delete: sets active=false. Data is preserved.\n  \
                     `--confirm` must repeat the slug verbatim — interactive\n  \
                     terminals can omit it and answer the prompt instead."
                        .into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "drop-tenant: unknown argument `{other}`"
                )));
            }
        }
    }
    let slug = match slug_arg {
        Some(s) => s,
        None => manage_interactive::ask("Tenant slug to drop: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("drop-tenant requires a slug positional argument".into())
            })?,
    };
    let confirm = match confirm {
        Some(c) => c,
        None => {
            // Interactive confirmation — make the user retype the
            // slug to prove they meant THIS tenant.
            let prompt = format!("Type `{slug}` to confirm soft-delete: ");
            manage_interactive::ask(&prompt)
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation(format!(
                        "drop-tenant requires `--confirm {slug}` (repeat the slug verbatim)"
                    ))
                })?
        }
    };
    if confirm != slug {
        return Err(TenancyError::Validation(format!(
            "drop-tenant: confirmation `{confirm}` does not match slug `{slug}` — aborted"
        )));
    }

    // The steps live in `tenancy::decommission` so the console runs
    // exactly these; this verb keeps what is a CLI's job — argv, the
    // confirmation prompt, and rendering.
    let report = super::super::decommission::decommission(
        pools,
        &slug,
        super::super::decommission::Action::Deactivate,
    )
    .await
    .map_err(|e| TenancyError::Validation(format!("drop-tenant: {e}")))?;
    if report.no_change {
        writeln!(w, "tenant `{slug}` already inactive — no change")?;
        return Ok(());
    }
    writeln!(
        w,
        "soft-deleted tenant `{slug}` (active=false). Data preserved."
    )?;
    writeln!(
        w,
        "  to hard-delete (drop schema or DB), use `purge-tenant`."
    )?;
    Ok(())
}

// ---------- purge-tenant (v0.6 step 6) ----------

pub(super) async fn purge_tenant<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(
        args,
        "purge-tenant",
        "slug",
        "purge-tenant <slug> [--confirm <slug>] [--purge-database]\n  \
         HARD-DELETE. Schema-mode: DROP SCHEMA <slug> CASCADE.\n  \
         Database-mode: refuses unless `--purge-database` is also\n  \
         passed; with it, runs `DROP DATABASE` against an admin\n  \
         connection. The Org row is deleted in both cases.\n  \
         Data is unrecoverable. Use `drop-tenant` for soft-delete.\n  \
         `--confirm` must repeat the slug verbatim — interactive\n  \
         terminals can omit it and answer the prompt instead.",
    )?;
    let mut iter = args.iter();
    let slug_arg = iter.next().cloned();
    let mut confirm: Option<String> = None;
    let mut purge_database = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--confirm" => {
                confirm = Some(next_value(&mut iter, "--confirm")?);
            }
            "--purge-database" => purge_database = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "purge-tenant <slug> [--confirm <slug>] [--purge-database]\n  \
                     HARD-DELETE. Schema-mode: DROP SCHEMA <slug> CASCADE.\n  \
                     Database-mode: refuses unless `--purge-database` is also\n  \
                     passed; with it, runs `DROP DATABASE` against an admin\n  \
                     connection. The Org row is deleted in both cases.\n  \
                     Data is unrecoverable. Use `drop-tenant` for soft-delete.\n  \
                     `--confirm` must repeat the slug verbatim — interactive\n  \
                     terminals can omit it and answer the prompt instead."
                        .into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "purge-tenant: unknown argument `{other}`"
                )));
            }
        }
    }
    let slug = match slug_arg {
        Some(s) => s,
        None => manage_interactive::ask("Tenant slug to PURGE: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("purge-tenant requires a slug positional argument".into())
            })?,
    };
    let confirm = match confirm {
        Some(c) => c,
        None => {
            // Interactive confirmation — make the operator retype the
            // slug to prove they meant THIS tenant. Mirrors drop-tenant
            // but the consequence is hard-delete, so the message is louder.
            let prompt = format!("HARD-DELETE: type `{slug}` to confirm permanent deletion: ");
            manage_interactive::ask(&prompt)
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation(format!(
                        "purge-tenant requires `--confirm {slug}` (repeat the slug verbatim)"
                    ))
                })?
        }
    };
    if confirm != slug {
        return Err(TenancyError::Validation(format!(
            "purge-tenant: confirmation `{confirm}` does not match slug `{slug}` — aborted"
        )));
    }

    // Everything destructive lives in `tenancy::decommission` so the
    // console runs exactly these steps. This verb keeps argv, the
    // confirmation prompt, and rendering.
    //
    // The flag check is here rather than there because `--purge-database`
    // is this CLI's vocabulary; the engine enforces the same rule in its
    // own words for every other caller.
    let registry = pools.registry_pool();
    let existing: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(&registry)
        .await?;
    let Some(org) = existing.into_iter().next() else {
        return Err(TenancyError::Validation(format!(
            "purge-tenant: no tenant with slug `{slug}`"
        )));
    };
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!("org `{slug}` has unknown storage_mode `{got}`"))
    })?;
    if mode == StorageMode::Database && !purge_database {
        return Err(TenancyError::Validation(format!(
            "tenant `{slug}` is database-mode — `DROP DATABASE` is unrecoverable. \
             Pass `--purge-database` to confirm you want the DB dropped, or use \
             `drop-tenant` for soft-delete."
        )));
    }

    let report = super::super::decommission::decommission(
        pools,
        &slug,
        super::super::decommission::Action::Purge { purge_database },
    )
    .await
    .map_err(|e| TenancyError::Validation(format!("purge-tenant: {e}")))?;

    if let Some(schema) = &report.schema_dropped {
        writeln!(w, "purged tenant `{slug}` (dropped schema `{schema}`)")?;
    }
    if report.database_dropped.is_some() {
        writeln!(w, "purged tenant `{slug}` (dropped dedicated database)")?;
    }
    for note in &report.notes {
        writeln!(w, "  {note}")?;
    }
    if report.row_deleted {
        writeln!(w, "  removed Org row")?;
    }
    Ok(())
}

// ---------- list-tenants ----------

pub(super) async fn list_tenants<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let orgs: Vec<Org> = Org::objects().fetch(&pools.registry_pool()).await?;
    if orgs.is_empty() {
        writeln!(w, "(no tenants)")?;
        return Ok(());
    }
    writeln!(
        w,
        "{:<24} {:<10} {:<32} {:<8} created_at",
        "slug", "mode", "host_pattern", "active"
    )?;
    writeln!(w, "{}", "-".repeat(80))?;
    for o in &orgs {
        writeln!(
            w,
            "{:<24} {:<10} {:<32} {:<8} {}",
            truncate(&o.slug, 24),
            o.storage_mode,
            o.host_pattern.as_deref().unwrap_or("-"),
            o.active,
            o.created_at.format("%Y-%m-%d %H:%M:%SZ"),
        )?;
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}
