//! Tenant provisioning + management subcommands.
//!
//! Composes with `rustango_migrate::manage::run` — recognizes the
//! tenancy-specific verbs (`create-tenant`, `drop-tenant`,
//! `list-tenants`, `migrate-tenants`) and delegates everything else
//! to the standard single-tenant runner using the registry pool.
//!
//! ## User wiring
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let registry_url = std::env::var("DATABASE_URL")?;
//!     let pool = rustango::sql::sqlx::PgPool::connect(&registry_url).await?;
//!     let pools = rustango_tenancy::TenantPools::new(pool);
//!     let dir = std::path::Path::new("./migrations");
//!     rustango_tenancy::manage::run(
//!         &pools,
//!         &registry_url,
//!         dir,
//!         std::env::args().skip(1),
//!     ).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Subcommands added in Slice 5
//!
//! | Verb               | Action |
//! |--------------------|--------|
//! | `create-tenant`    | Insert Org row, create schema (schema mode), run tenant migrations |
//! | `drop-tenant`      | Soft-delete (sets `active = false`); data is preserved for recovery |
//! | `list-tenants`     | Print all orgs in a table |
//! | `migrate-tenants`  | Run pending tenant-scoped migrations across active orgs |
//! | (anything else)    | Delegated to `rustango_migrate::manage::run` (registry-scoped) |
//!
//! Hard-delete with schema/DB drop lands as the v0.6 `purge-tenant
//! <slug> --confirm <slug>` verb. Schema-mode tenants get
//! `DROP SCHEMA … CASCADE`; database-mode tenants additionally
//! require `--purge-database` and run `DROP DATABASE …` against an
//! admin connection. The Org row is deleted in both cases.

use std::io::{self, Write};
use std::path::Path;

use rustango::core::Column as _;
use rustango::sql::{Auto, Fetcher, Updater};

use crate::error::TenancyError;
use crate::migrate as tenant_migrate;
use crate::org::{Org, StorageMode};
use crate::pools::TenantPools;

/// Dispatch entrypoint. Recognizes tenancy verbs and delegates the
/// rest to `rustango_migrate::manage::run`.
///
/// `dir` is the migrations directory (same as the underlying
/// rustango-migrate runner). `registry_url` is needed for schema-
/// mode tenant migrations + tenant pool building; supply the same
/// value the registry pool was built from.
///
/// # Errors
/// Either a [`TenancyError`] from a tenancy verb, or a wrapped
/// `rustango_migrate::MigrateError` from the delegated call.
pub async fn run(
    pools: &TenantPools,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
) -> Result<(), TenancyError> {
    let mut stdout = io::stdout();
    run_with_writer(pools, registry_url, dir, args, &mut stdout).await
}

/// Same as [`run`] but writes user-facing output to `writer` —
/// useful for tests.
///
/// # Errors
/// As [`run`].
pub async fn run_with_writer<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
    writer: &mut W,
) -> Result<(), TenancyError> {
    let args: Vec<String> = args.into_iter().collect();
    let cmd = args.first().map_or("", String::as_str);

    match cmd {
        "create-tenant" => create_tenant(pools, registry_url, dir, &args[1..], writer).await,
        "drop-tenant" => drop_tenant(pools, &args[1..], writer).await,
        "purge-tenant" => purge_tenant(pools, &args[1..], writer).await,
        "list-tenants" => list_tenants(pools, writer).await,
        "migrate-tenants" => migrate_tenants_cmd(pools, registry_url, dir, writer).await,
        "migrate-registry" => migrate_registry_cmd(pools, dir, writer).await,
        "create-operator" => create_operator_cmd(pools, &args[1..], writer).await,
        "create-user" => create_user_cmd(pools, registry_url, &args[1..], writer).await,
        "run-server" | "runserver" => run_server_cmd(pools, registry_url, &args[1..], writer).await,
        "init-tenancy" => init_tenancy_cmd(dir, writer),
        // Plain `migrate` is scope-aware here — registry-scoped
        // migrations apply to the registry pool first, then tenant-
        // scoped ones fan out across active orgs. Direct fall-through
        // to `rustango::migrate::manage` (which is scope-blind) would
        // apply tenant migrations to the registry pool, a real
        // footgun. `migrate-registry` / `migrate-tenants` stay
        // available for fine-grained control.
        "migrate" => migrate_all_cmd(pools, registry_url, dir, &args[1..], writer).await,
        // Everything else (makemigrations, showmigrations, downgrade,
        // help, …) is registry-scoped and delegates to the standard
        // single-tenant runner.
        _ => rustango::migrate::manage::run_with_writer(
            pools.registry(),
            dir,
            args,
            writer,
        )
        .await
        .map_err(TenancyError::Migrate),
    }
}

// ---------- create-tenant ----------

struct CreateTenantArgs {
    slug: String,
    mode: StorageMode,
    display_name: Option<String>,
    database_url: Option<String>,
    schema_name: Option<String>,
    host_pattern: Option<String>,
    port: Option<i32>,
    path_prefix: Option<String>,
    no_migrate: bool,
}

async fn create_tenant<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let parsed = parse_create_tenant_args(args)?;

    // Reject duplicate slug up front — saves a partial-state mess
    // when CREATE SCHEMA succeeds and the INSERT then fails.
    let existing: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(parsed.slug.clone()))
        .fetch(pools.registry())
        .await?;
    if !existing.is_empty() {
        return Err(TenancyError::Validation(format!(
            "tenant slug `{}` already exists",
            parsed.slug
        )));
    }

    // Compute defaults that depend on the slug + apex env var.
    let host_pattern = parsed.host_pattern.clone().or_else(|| {
        std::env::var("RUSTANGO_APEX_DOMAIN")
            .ok()
            .map(|apex| format!("{}.{apex}", parsed.slug))
    });
    let display_name = parsed
        .display_name
        .clone()
        .unwrap_or_else(|| parsed.slug.clone());
    let schema_name = match parsed.mode {
        StorageMode::Schema => Some(parsed.schema_name.clone().unwrap_or_else(|| parsed.slug.clone())),
        StorageMode::Database => None,
    };

    if parsed.mode == StorageMode::Database && parsed.database_url.is_none() {
        return Err(TenancyError::Validation(
            "create-tenant --mode database requires --database-url".into(),
        ));
    }

    // Schema-mode: create the schema before inserting the row so
    // a failed INSERT doesn't leave an orphan schema. Idempotent
    // via IF NOT EXISTS.
    if let StorageMode::Schema = parsed.mode {
        let schema = schema_name.as_deref().unwrap_or(&parsed.slug);
        let sql = format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_ident(schema)
        );
        rustango::sql::sqlx::query(&sql)
            .execute(pools.registry())
            .await?;
    }

    let mut org = Org {
        id: Auto::default(),
        slug: parsed.slug.clone(),
        display_name,
        storage_mode: parsed.mode.as_str().into(),
        database_url: parsed.database_url.clone(),
        schema_name,
        host_pattern,
        port: parsed.port,
        path_prefix: parsed.path_prefix.clone(),
        active: true,
        created_at: chrono::Utc::now(),
    };
    org.insert(pools.registry()).await?;
    let id = org.id.get().copied().unwrap_or_default();
    writeln!(
        w,
        "created tenant `{}` (id {id}, mode {})",
        parsed.slug,
        parsed.mode
    )?;

    // Run tenant migrations against the freshly-provisioned tenant
    // unless --no-migrate.
    if parsed.no_migrate {
        writeln!(w, "  --no-migrate: skipping tenant migrations")?;
        return Ok(());
    }
    writeln!(w, "  applying tenant migrations…")?;
    let report = tenant_migrate::migrate_tenants(pools, dir, registry_url).await?;
    let outcome = report.tenants.iter().find(|t| t.slug == parsed.slug);
    match outcome {
        Some(o) => {
            if let Some(err) = &o.error {
                writeln!(w, "  migration failed: {err}")?;
            } else {
                writeln!(w, "  applied {} migration(s)", o.applied.len())?;
                for m in &o.applied {
                    writeln!(w, "    + {}", m.name)?;
                }
            }
        }
        None => writeln!(w, "  no migrations matched this tenant")?,
    }
    Ok(())
}

fn parse_create_tenant_args(args: &[String]) -> Result<CreateTenantArgs, TenancyError> {
    let mut iter = args.iter();
    let slug_arg = iter.next().cloned();
    let slug = match slug_arg {
        Some(s) => s,
        None => crate::manage_interactive::ask("Tenant slug: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "create-tenant requires a slug positional argument".into(),
                )
            })?,
    };
    let mut out = CreateTenantArgs {
        slug,
        mode: StorageMode::Schema,
        display_name: None,
        database_url: None,
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        no_migrate: false,
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
            "--no-migrate" => out.no_migrate = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "create-tenant <slug> [--mode schema|database] [--display-name <s>] \
                     [--database-url <url>] [--schema-name <s>] [--host-pattern <s>] \
                     [--port <n>] [--path-prefix <s>] [--no-migrate]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-tenant: unknown argument `{other}`"
                )));
            }
        }
    }
    Ok(out)
}

fn next_value<'a, I: Iterator<Item = &'a String>>(
    iter: &mut I,
    flag: &str,
) -> Result<String, TenancyError> {
    iter.next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(format!("{flag} requires a value")))
}

// ---------- drop-tenant ----------

async fn drop_tenant<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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
                     terminals can omit it and answer the prompt instead.".into(),
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
        None => crate::manage_interactive::ask("Tenant slug to drop: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "drop-tenant requires a slug positional argument".into(),
                )
            })?,
    };
    let confirm = match confirm {
        Some(c) => c,
        None => {
            // Interactive confirmation — make the user retype the
            // slug to prove they meant THIS tenant.
            let prompt = format!("Type `{slug}` to confirm soft-delete: ");
            crate::manage_interactive::ask(&prompt)
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

    let existing: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(pools.registry())
        .await?;
    let Some(org) = existing.into_iter().next() else {
        return Err(TenancyError::Validation(format!(
            "drop-tenant: no tenant with slug `{slug}`"
        )));
    };
    if !org.active {
        writeln!(w, "tenant `{slug}` already inactive — no change")?;
        return Ok(());
    }

    // Soft-delete: UPDATE rustango_orgs SET active = false WHERE id = $1.
    let id = org.id.get().copied().ok_or_else(|| {
        TenancyError::Validation("dropped Org row has no PK".into())
    })?;
    let updated = Org::objects()
        .where_(Org::id.eq(id))
        .update()
        .set("active", false)
        .execute(pools.registry())
        .await?;
    if updated == 0 {
        return Err(TenancyError::Validation(format!(
            "drop-tenant: no row updated for id {id} — race condition?"
        )));
    }
    writeln!(w, "soft-deleted tenant `{slug}` (active=false). Data preserved.")?;
    writeln!(
        w,
        "  to hard-delete (drop schema or DB), use `purge-tenant`."
    )?;
    Ok(())
}

// ---------- purge-tenant (v0.6 step 6) ----------

async fn purge_tenant<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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
        None => crate::manage_interactive::ask("Tenant slug to PURGE: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "purge-tenant requires a slug positional argument".into(),
                )
            })?,
    };
    let confirm = match confirm {
        Some(c) => c,
        None => {
            // Interactive confirmation — make the operator retype the
            // slug to prove they meant THIS tenant. Mirrors drop-tenant
            // but the consequence is hard-delete, so the message is louder.
            let prompt = format!(
                "HARD-DELETE: type `{slug}` to confirm permanent deletion: "
            );
            crate::manage_interactive::ask(&prompt)
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

    let existing: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(pools.registry())
        .await?;
    let Some(org) = existing.into_iter().next() else {
        return Err(TenancyError::Validation(format!(
            "purge-tenant: no tenant with slug `{slug}`"
        )));
    };

    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{slug}` has unknown storage_mode `{got}`"
        ))
    })?;

    match mode {
        StorageMode::Schema => {
            let schema = org.schema_name.clone().unwrap_or_else(|| slug.clone());
            let sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(&schema));
            rustango::sql::sqlx::query(&sql)
                .execute(pools.registry())
                .await?;
            writeln!(w, "purged tenant `{slug}` (dropped schema `{schema}`)")?;
        }
        StorageMode::Database => {
            if !purge_database {
                return Err(TenancyError::Validation(format!(
                    "tenant `{slug}` is database-mode — `DROP DATABASE` is unrecoverable. \
                     Pass `--purge-database` to confirm you want the DB dropped, or use \
                     `drop-tenant` for soft-delete."
                )));
            }
            // Resolve the URL through the secrets resolver so vault-
            // backed orgs purge correctly. Then close & drop the
            // cached pool — DROP DATABASE refuses while connections
            // are open.
            let url = pools.resolved_database_url(&org).await?;
            pools.invalidate(&slug).await;
            drop_database_at(&url, w).await?;
            writeln!(w, "purged tenant `{slug}` (dropped dedicated database)")?;
        }
    }

    // DELETE the Org row. Use a raw query so we don't depend on a
    // model-level delete API (rustango doesn't ship one yet).
    let id = org.id.get().copied().ok_or_else(|| {
        TenancyError::Validation("purge-tenant: Org row has no PK".into())
    })?;
    let result = rustango::sql::sqlx::query("DELETE FROM rustango_orgs WHERE id = $1")
        .bind(id)
        .execute(pools.registry())
        .await?;
    if result.rows_affected() == 0 {
        return Err(TenancyError::Validation(format!(
            "purge-tenant: no Org row deleted for id {id} — race condition?"
        )));
    }
    writeln!(w, "  removed Org row (id {id})")?;
    Ok(())
}

/// Connect to the same Postgres server as `tenant_url` but switch to
/// the `postgres` admin database (DROP DATABASE can't run from a
/// connection to the database being dropped). Issue the DROP, then
/// close the admin connection.
///
/// Inherits credentials, host, port, and TLS settings from
/// `tenant_url`. The dropped database is `tenant_url`'s
/// `database` field — same DB the tenant pool was connected to.
async fn drop_database_at<W: Write + Send>(
    tenant_url: &str,
    w: &mut W,
) -> Result<(), TenancyError> {
    use rustango::sql::sqlx::postgres::PgConnectOptions;
    use rustango::sql::sqlx::ConnectOptions;
    use std::str::FromStr;

    let opts = PgConnectOptions::from_str(tenant_url).map_err(|e| {
        TenancyError::Validation(format!(
            "purge-tenant: cannot parse database_url `{tenant_url}`: {e}"
        ))
    })?;
    let dbname = opts.get_database().ok_or_else(|| {
        TenancyError::Validation(
            "purge-tenant: database_url is missing the database name — \
             can't determine what to DROP DATABASE"
                .into(),
        )
    })?;
    if dbname.eq_ignore_ascii_case("postgres") || dbname.eq_ignore_ascii_case("template0")
        || dbname.eq_ignore_ascii_case("template1")
    {
        return Err(TenancyError::Validation(format!(
            "purge-tenant: refusing to DROP DATABASE `{dbname}` (Postgres system database)"
        )));
    }
    let dbname = dbname.to_owned();
    let admin_opts = opts.clone().database("postgres");
    let mut admin = admin_opts.connect().await?;
    let sql = format!("DROP DATABASE IF EXISTS {}", quote_ident(&dbname));
    writeln!(w, "  issuing {sql}")?;
    rustango::sql::sqlx::query(&sql)
        .execute(&mut admin)
        .await?;
    Ok(())
}

// ---------- list-tenants ----------

async fn list_tenants<W: Write + Send>(
    pools: &TenantPools,
    w: &mut W,
) -> Result<(), TenancyError> {
    let orgs: Vec<Org> = Org::objects().fetch(pools.registry()).await?;
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

// ---------- migrate-tenants ----------

async fn migrate_tenants_cmd<W: Write + Send>(
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
    report: &crate::migrate::TenantMigrationReport,
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

async fn migrate_registry_cmd<W: Write + Send>(
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

async fn migrate_all_cmd<W: Write + Send>(
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

fn init_tenancy_cmd<W: Write>(dir: &Path, w: &mut W) -> Result<(), TenancyError> {
    let report = crate::bootstrap::init_tenancy(dir)?;
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

fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

// ---------- create-operator (Slice 6) ----------

async fn create_operator_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut iter = args.iter();
    let username_arg = iter.next().cloned();
    let mut password: Option<String> = None;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "create-operator <username> --password <p>".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-operator: unknown argument `{other}`"
                )));
            }
        }
    }
    // Prompt for missing values when stdin is a TTY; programmatic
    // callers that pass `None` on a non-interactive stream still get
    // the original Validation error.
    let username = match username_arg {
        Some(u) => u,
        None => crate::manage_interactive::ask("Username: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "create-operator requires a username positional argument".into(),
                )
            })?,
    };
    let plain = match password {
        Some(p) => p,
        None => crate::manage_interactive::ask_password("Password: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("create-operator requires --password".into())
            })?,
    };

    // Reject duplicate username up front.
    let existing: Vec<crate::Operator> = crate::Operator::objects()
        .where_(crate::Operator::username.eq(username.clone()))
        .fetch(pools.registry())
        .await?;
    if !existing.is_empty() {
        return Err(TenancyError::Validation(format!(
            "operator `{username}` already exists in the registry"
        )));
    }

    let mut op = crate::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: crate::password::hash(&plain)?,
        active: true,
        created_at: chrono::Utc::now(),
    };
    op.insert(pools.registry()).await?;
    let id = op.id.get().copied().unwrap_or_default();
    writeln!(w, "created operator `{username}` (id {id})")?;
    Ok(())
}

// ---------- create-user (Slice 6) ----------

async fn create_user_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut iter = args.iter();
    let slug_arg = iter.next().cloned();
    let username_arg = iter.next().cloned();
    let mut password: Option<String> = None;
    let mut is_superuser = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--superuser" => is_superuser = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "create-user <slug> <username> --password <p> [--superuser]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-user: unknown argument `{other}`"
                )));
            }
        }
    }
    let slug = match slug_arg {
        Some(s) => s,
        None => crate::manage_interactive::ask("Tenant slug: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "create-user requires a tenant slug as the first positional argument".into(),
                )
            })?,
    };
    let username = match username_arg {
        Some(u) => u,
        None => crate::manage_interactive::ask("Username: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "create-user requires a username as the second positional argument".into(),
                )
            })?,
    };
    let plain = match password {
        Some(p) => p,
        None => crate::manage_interactive::ask_password("Password: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("create-user requires --password".into())
            })?,
    };

    // Look up the tenant.
    let orgs: Vec<crate::Org> = crate::Org::objects()
        .where_(crate::Org::slug.eq(slug.clone()))
        .fetch(pools.registry())
        .await?;
    let org = orgs.into_iter().next().ok_or_else(|| {
        TenancyError::Validation(format!("create-user: no tenant with slug `{slug}`"))
    })?;

    let hash = crate::password::hash(&plain)?;
    let now = chrono::Utc::now().to_rfc3339();

    // We bypass `User::insert` because that uses `pools.registry()`'s
    // pool by default and we need a connection scoped to the tenant.
    // Hand-write an INSERT against the scoped connection.
    use crate::org::StorageMode;
    use rustango::sql::sqlx::Row;
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{slug}` has unknown storage_mode `{got}`"
        ))
    })?;
    let row_id: i64 = match mode {
        StorageMode::Schema => {
            // Fresh search-path-bound pool so the INSERT lands in the
            // tenant's schema. Mirrors the migration / admin path.
            let schema = org.schema_name.clone().unwrap_or_else(|| slug.clone());
            let pool = build_schema_scoped_pool(registry_url, &schema).await?;
            let row = rustango::sql::sqlx::query(
                "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
                 VALUES ($1, $2, $3, true, $4::timestamptz) RETURNING id",
            )
            .bind(&username)
            .bind(&hash)
            .bind(is_superuser)
            .bind(&now)
            .fetch_one(&pool)
            .await?;
            let id: i64 = row.try_get("id")?;
            pool.close().await;
            id
        }
        StorageMode::Database => {
            let tp = pools.pool_for_org(&org).await?;
            let row = rustango::sql::sqlx::query(
                "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
                 VALUES ($1, $2, $3, true, $4::timestamptz) RETURNING id",
            )
            .bind(&username)
            .bind(&hash)
            .bind(is_superuser)
            .bind(&now)
            .fetch_one(tp.pool())
            .await?;
            row.try_get("id")?
        }
    };
    writeln!(
        w,
        "created user `{username}` in tenant `{slug}` (id {row_id}, superuser={is_superuser})"
    )?;
    Ok(())
}

// ---------- run-server (Slice operator-console-runserver) ----------

async fn run_server_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut cfg = crate::server::ServerConfig::from_env();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--bind" => cfg.bind = next_value(&mut iter, "--bind")?,
            "--apex" | "--apex-domain" => {
                cfg.apex_domain = next_value(&mut iter, "--apex")?;
            }
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "run-server [--bind <addr>] [--apex <domain>]\n  \
                     Boots the operator console (apex) + tenant admin\n  \
                     (subdomains) with sensible defaults. Reads RUSTANGO_BIND,\n  \
                     RUSTANGO_APEX_DOMAIN, RUSTANGO_SESSION_SECRET from env.\n  \
                     Ctrl-C to stop."
                        .into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "run-server: unknown argument `{other}`"
                )));
            }
        }
    }
    // Pools is borrowed; the server takes an `Arc<TenantPools>` so it
    // can clone into per-request closures. Build a fresh Arc carrying
    // a clone of the registry pool — the existing pools' database-mode
    // cache stays distinct, but for `run-server` the freshly-built
    // registry uses the same connection-pool handle so the existing
    // cache isn't lost.
    let arc_pools = std::sync::Arc::new(crate::TenantPools::new(pools.registry().clone()));
    crate::server::run(arc_pools, registry_url.to_owned(), cfg, w).await
}

/// Mirror of the migration helper — build a short-lived pool whose
/// connections have `search_path` pre-set. Local copy so manage
/// doesn't need a public reference into [`crate::migrate`].
async fn build_schema_scoped_pool(
    registry_url: &str,
    schema: &str,
) -> Result<rustango::sql::sqlx::PgPool, TenancyError> {
    use rustango::sql::sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;
    let schema_owned: Arc<str> = Arc::from(schema);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _meta| {
            let schema = Arc::clone(&schema_owned);
            Box::pin(async move {
                let stmt = format!(
                    "SET search_path TO {}, public",
                    quote_ident(&schema)
                );
                rustango::sql::sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect(registry_url)
        .await?;
    Ok(pool)
}
