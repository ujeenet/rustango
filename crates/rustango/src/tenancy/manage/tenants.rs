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
use crate::tenancy::provision::{self, DatabaseSource, TenantStorage};

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
    // Before anything is made: a missing prod key must not strand an admin.
    let handover = match &request.first_admin {
        Some(_) => Some(cli_handover(w)?),
        None => None,
    };

    // Steps print as they happen rather than from the outcome, because
    // "created tenant …" has always appeared *before* the migrations,
    // and the summary after them.
    let progress = CreateTenantProgress {
        out: std::sync::Mutex::new(w),
        slug: request.slug.clone(),
        mode: request.mode(),
    };
    // Recorded, like the console's provisioning (#1344). A tenant created
    // from a shell used to leave no run at all, so the run history — and
    // `list-runs` — described only what the console had done, and a CLI
    // provision that died halfway left nothing to find. Recording is
    // best-effort inside `provision_tenant_recorded`: bookkeeping must not
    // be what fails a tenant creation.
    let (_run, mut outcome) = provision::provision_tenant_recorded(
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
    if let Some(handover) = &handover {
        let handed = handover
            .hand_over_outcome(&pools.registry_pool(), &mut outcome, None)
            .await;
        if let Some(admin) = &outcome.first_admin {
            match (&handed, &admin.handover) {
                (Ok(()), Some(link)) => {
                    print_handover(w, &admin.username, link, handover.ttl_hours())?
                }
                // The tenant is made; only the link is lost.
                _ => writeln!(
                    w,
                    "  could not issue the set-password link; run `set-password-link {} {}`",
                    outcome.slug, admin.username
                )?,
            }
        }
    }
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
            E::Step {
                step: S::CreateDatabase,
                status: StepStatus::Ok,
            } => writeln!(out, "created database `{}`", self.slug),
            E::Step {
                step: S::CreateLogin,
                status: StepStatus::Ok,
            } => writeln!(out, "  created its own login"),
            E::Step {
                step: S::Grant,
                status: StepStatus::Ok,
            } => writeln!(out, "  granted the login that database only"),
            E::Step {
                step: S::CreateAdmin,
                status: StepStatus::Ok,
            } => writeln!(out, "  created the first admin (superuser)"),
            E::Step {
                step: S::Rollback,
                status: StepStatus::Ok,
            } => writeln!(out, "rolled back: dropped the new database and login"),
            E::Step {
                step: S::Rollback,
                status: StepStatus::Failed(why),
            } => writeln!(out, "rollback failed: {why}"),
            // Validation and storage failures come back as `Err` from
            // the engine and are rendered by the caller; the remaining
            // steps are detail the CLI has never printed.
            _ => Ok(()),
        };
        let _ = out.flush();
    }
}

/// The CLI signs links with the server's tenant session key, loaded only.
fn cli_handover<W: Write>(w: &mut W) -> Result<crate::tenancy::FirstAdminHandover, TenancyError> {
    use crate::tenancy::session::{existing_tenant_secret, KeySource};
    let (secret, source) = existing_tenant_secret()?;
    if source == KeySource::DevFile {
        writeln!(
            w,
            "  note: links are signed with the dev key file; they work only on a server using the same file"
        )?;
    }
    Ok(crate::tenancy::FirstAdminHandover::new(&secret))
}

/// Print the link once. It is never logged or stored.
fn print_handover<W: Write>(
    w: &mut W,
    username: &str,
    handed: &crate::tenancy::Handover,
    hours: u64,
) -> Result<(), TenancyError> {
    if let crate::tenancy::Handover::Mailed { to } = handed {
        writeln!(w, "  set-password link for `{username}` mailed to {to}")?;
    } else if let Some(link) = handed.link() {
        writeln!(
            w,
            "  set-password link for `{username}` (works once, expires in {hours}h):"
        )?;
        writeln!(w, "  {}", link.expose())?;
        writeln!(w, "  give it to the admin; it won't be shown again")?;
    }
    Ok(())
}

// ---------- set-password-link ----------

/// `set-password-link <slug> <username>`: a fresh single-use link, for an
/// expired one or a mail that never arrived.
pub(super) async fn set_password_link<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    const USAGE: &str = "set-password-link <slug> <username>";
    let parsed = super::args::parse(
        args,
        &super::args::Spec {
            verb: "set-password-link",
            usage: USAGE,
            switches: &[],
            valued: &[],
            max_positionals: 2,
        },
    )?;
    let slug = parsed.required(0, USAGE)?;
    let username = parsed.required(1, USAGE)?;
    let org = super::api::find_org(pools, &slug)
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("no tenant with slug `{slug}`")))?;
    let handover = cli_handover(w)?;
    let handed =
        crate::tenancy::first_admin::hand_over_user(pools, &org, &username, &handover).await?;
    print_handover(w, &username, &handed, handover.ttl_hours())
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
        [--database-url <url> | --server <name>] [--schema-name <s>] [--host-pattern <s>] \
        [--port <n>] [--path-prefix <s>] [--no-migrate] \
        [--admin-email <e> [--admin-username <u>]]";
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
    let mut out = provision::ProvisionRequest::new(slug, TenantStorage::Schema(None));
    let mut mode = None;
    let (mut database_url, mut server, mut schema_name) = (None, None, None);
    let (mut admin_email, mut admin_username) = (None, None);
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--mode" => {
                let v = next_value(&mut iter, "--mode")?;
                mode = Some(StorageMode::parse(&v).map_err(|got| {
                    TenancyError::Validation(format!(
                        "--mode must be `schema` or `database`, got `{got}`"
                    ))
                })?);
            }
            "--backend" => {
                let v = next_value(&mut iter, "--backend")?;
                out.backend = Some(BackendKind::parse(&v).map_err(|got| {
                    TenancyError::Validation(format!(
                        "--backend must be `postgres`, `mysql`, or `sqlite`, got `{got}`"
                    ))
                })?);
            }
            "--display-name" => out.display_name = Some(next_value(&mut iter, "--display-name")?),
            "--database-url" => database_url = Some(next_value(&mut iter, "--database-url")?),
            "--server" => server = Some(next_value(&mut iter, "--server")?),
            "--schema-name" => schema_name = Some(next_value(&mut iter, "--schema-name")?),
            "--host-pattern" => out.host_pattern = Some(next_value(&mut iter, "--host-pattern")?),
            "--port" => {
                let v = next_value(&mut iter, "--port")?;
                out.port = Some(v.parse().map_err(|_| {
                    TenancyError::Validation(format!("--port must be an integer, got `{v}`"))
                })?);
            }
            "--path-prefix" => out.path_prefix = Some(next_value(&mut iter, "--path-prefix")?),
            "--no-migrate" => out.run_migrations = false,
            "--admin-email" => admin_email = Some(next_value(&mut iter, "--admin-email")?),
            "--admin-username" => {
                admin_username = Some(next_value(&mut iter, "--admin-username")?);
            }
            "--help" | "-h" => return Err(TenancyError::Validation(HELP.to_owned())),
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-tenant: unknown argument `{other}`"
                )));
            }
        }
    }
    // A database flag alone means database mode.
    let mode = mode.unwrap_or(if database_url.is_some() || server.is_some() {
        StorageMode::Database
    } else {
        StorageMode::Schema
    });
    let bad = |m: &str| Err(TenancyError::Validation(format!("create-tenant: {m}")));
    out.first_admin = match (admin_email, admin_username) {
        (Some(email), username) => Some(
            crate::tenancy::FirstAdmin::new(&email, username.as_deref())
                .map_err(|m| TenancyError::Validation(format!("create-tenant: {m}")))?,
        ),
        (None, Some(_)) => return bad("--admin-username needs --admin-email"),
        (None, None) => None,
    };
    out.storage = match (mode, database_url, server) {
        (StorageMode::Schema, None, None) => TenantStorage::Schema(schema_name),
        (StorageMode::Schema, _, _) => {
            return bad("--database-url and --server need --mode database")
        }
        (StorageMode::Database, Some(_), Some(_)) => {
            return bad("give --database-url or --server, not both")
        }
        (StorageMode::Database, Some(url), None) => {
            TenantStorage::Database(DatabaseSource::Url(url))
        }
        (StorageMode::Database, None, Some(name)) => {
            TenantStorage::Database(DatabaseSource::Server(name))
        }
        (StorageMode::Database, None, None) => {
            return bad("database mode needs --database-url <url> or --server <name>")
        }
    };
    // v0.33 — guard the unsupported pairs early so a clear error
    // surfaces instead of a generic schema-mode failure deep in the
    // pool layer.
    if let Some(backend) = out.backend {
        backend
            .validate_storage_mode(mode)
            .map_err(|msg| TenancyError::Validation(msg.to_owned()))?;
    }
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

    let progress = OperationProgress::new(w);
    let purged = super::super::decommission::purge_recorded(
        pools,
        &slug,
        purge_database,
        Some(&progress),
        Some("cli"),
    )
    .await;
    let w = progress.into_inner();
    let (_run, report) =
        purged.map_err(|e| TenancyError::Validation(format!("purge-tenant: {e}")))?;

    if let Some(schema) = &report.schema_dropped {
        writeln!(w, "purged tenant `{slug}` (dropped schema `{schema}`)")?;
    }
    if report.database_dropped.is_some() {
        writeln!(w, "purged tenant `{slug}` (dropped dedicated database)")?;
    }
    if report.media_deleted > 0 {
        writeln!(w, "  deleted {} media objects", report.media_deleted)?;
    }
    for note in &report.notes {
        writeln!(w, "  {note}")?;
    }
    for (disk, key) in &report.media_left {
        writeln!(w, "  media left in storage: disk `{disk}`, key `{key}`")?;
    }
    if report.row_deleted {
        writeln!(w, "  removed Org row")?;
    }
    // The CLI sees only the default local store, not one the app injected.
    let root = super::super::branding::brand_storage_root();
    let brand = super::super::branding::default_brand_storage();
    match super::super::branding::delete_brand_assets(&slug, &brand).await {
        Ok(()) => writeln!(
            w,
            "  removed brand files under `{}`; with a custom brand store, delete `{slug}/` there",
            root.display()
        )?,
        Err(e) => writeln!(w, "  brand files left in place: {e}")?,
    }
    Ok(())
}

// ---------- rotate-tenant-password ----------

pub(super) async fn rotate_tenant_password<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    const HELP: &str = "rotate-tenant-password <slug>\n  \
        A new password for the login rustango made for the tenant on a saved\n  \
        server: altered on the server, then sealed and stored, then pools reopen.";
    reject_leading_flag(args, "rotate-tenant-password", "slug", HELP)?;
    let slug = match args {
        [slug] => slug.clone(),
        _ => return Err(TenancyError::Validation(HELP.to_owned())),
    };
    let progress = OperationProgress::new(w);
    let rotated = crate::tenancy::server_tenant::rotate_password_recorded(
        pools,
        &slug,
        Some(&progress),
        Some("cli"),
    )
    .await;
    let w = progress.into_inner();
    rotated.map_err(|e| TenancyError::Validation(format!("rotate-tenant-password: {e}")))?;
    writeln!(w, "rotated the database password of tenant `{slug}`")?;
    Ok(())
}

/// Prints each step of a purge or rotation as it ends, as the console's
/// run page shows it. Never prints a password: events carry none.
struct OperationProgress<'w, W> {
    out: std::sync::Mutex<&'w mut W>,
}

impl<'w, W: Write + Send> OperationProgress<'w, W> {
    fn new(out: &'w mut W) -> Self {
        Self {
            out: std::sync::Mutex::new(out),
        }
    }

    fn into_inner(self) -> &'w mut W {
        self.out.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W: Write + Send> crate::tenancy::operation::OperationObserver for OperationProgress<'_, W> {
    fn on_event(&self, event: crate::tenancy::operation::OperationEvent) {
        use provision::StepStatus;
        let Ok(mut out) = self.out.lock() else {
            return;
        };
        let step = event.step.as_str();
        // Progress is cosmetic; a broken pipe must not fail the verb.
        let _ = match &event.status {
            StepStatus::Started => Ok(()),
            StepStatus::Ok => writeln!(out, "  {step} ... ok"),
            StepStatus::Skipped(why) => writeln!(out, "  {step} ... skipped ({why})"),
            StepStatus::Failed(why) => writeln!(out, "  {step} ... failed: {why}"),
        };
        let _ = out.flush();
    }
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
