//! Tenant-lifecycle verbs: `create-tenant`, `drop-tenant`,
//! `purge-tenant`, `list-tenants`. Plus their parsers + the
//! database-mode admin-DROP helper.

use std::io::Write;
use std::path::Path;

use sqlx::Database;

use crate::core::Column as _;
use crate::sql::{Auto, FetcherPool, UpdaterPool};

use crate::tenancy::error::TenancyError;
#[cfg(feature = "postgres")]
use crate::tenancy::manage::args::quote_ident;
use crate::tenancy::manage::args::{next_value, reject_leading_flag};
use crate::tenancy::manage_interactive;
use crate::tenancy::migrate as tenant_migrate;
use crate::tenancy::org::{BackendKind, Org, StorageMode};
use crate::tenancy::pools::TenantPools;

// ---------- create-tenant ----------

struct CreateTenantArgs {
    slug: String,
    mode: StorageMode,
    backend: BackendKind,
    display_name: Option<String>,
    database_url: Option<String>,
    schema_name: Option<String>,
    host_pattern: Option<String>,
    port: Option<i32>,
    path_prefix: Option<String>,
    no_migrate: bool,
}

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
    let parsed = parse_create_tenant_args(args)?;

    // Reject duplicate slug up front — saves a partial-state mess
    // when CREATE SCHEMA succeeds and the INSERT then fails.
    let registry = pools.registry_pool();
    let existing: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(parsed.slug.clone()))
        .fetch_pool(&registry)
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
        StorageMode::Schema => Some(
            parsed
                .schema_name
                .clone()
                .unwrap_or_else(|| parsed.slug.clone()),
        ),
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
        // Schema mode is PG-only by language — `CREATE SCHEMA` /
        // `SET search_path` don't exist on sqlite or mysql.
        #[cfg(feature = "postgres")]
        {
            let pg_pools = (pools as &dyn std::any::Any)
                .downcast_ref::<TenantPools<sqlx::Postgres>>()
                .ok_or_else(|| {
                    TenancyError::Validation(
                        "schema-mode tenants require a Postgres registry — pass --mode database \
                         on sqlite/mysql"
                            .into(),
                    )
                })?;
            let schema = schema_name.as_deref().unwrap_or(&parsed.slug);
            let sql = format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema));
            rustango::sql::sqlx::query(&sql)
                .execute(pg_pools.registry())
                .await?;
        }
        #[cfg(not(feature = "postgres"))]
        {
            return Err(TenancyError::Validation(
                "schema-mode tenants require the `postgres` feature — pass --mode database \
                 on sqlite/mysql builds"
                    .into(),
            ));
        }
    }

    let mut org = Org {
        id: Auto::default(),
        slug: parsed.slug.clone(),
        display_name,
        storage_mode: parsed.mode.as_str().into(),
        backend_kind: parsed.backend.as_str().into(),
        database_url: parsed.database_url.clone(),
        schema_name,
        host_pattern,
        port: parsed.port,
        path_prefix: parsed.path_prefix.clone(),
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&registry).await?;
    let id = org.id.get().copied().unwrap_or_default();
    writeln!(
        w,
        "created tenant `{}` (id {id}, mode {})",
        parsed.slug, parsed.mode
    )?;

    // Run tenant migrations against the freshly-provisioned tenant
    // unless --no-migrate.
    if parsed.no_migrate {
        writeln!(w, "  --no-migrate: skipping tenant migrations")?;
        return Ok(());
    }
    writeln!(w, "  applying tenant migrations…")?;
    // v0.38 — on PG go through `migrate_tenants` (schema-mode +
    // database-mode); on sqlite/mysql use `migrate_tenants_db`
    // (database-mode only).
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
    let mut out = CreateTenantArgs {
        slug,
        mode: StorageMode::Schema,
        backend: BackendKind::Postgres,
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
            "--no-migrate" => out.no_migrate = true,
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

    let registry = pools.registry_pool();
    let existing: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch_pool(&registry)
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
    let id = org
        .id
        .get()
        .copied()
        .ok_or_else(|| TenancyError::Validation("dropped Org row has no PK".into()))?;
    let updated = Org::objects()
        .where_(Org::id.eq(id))
        .update()
        .set("active", false)
        .execute_pool(&registry)
        .await?;
    if updated == 0 {
        return Err(TenancyError::Validation(format!(
            "drop-tenant: no row updated for id {id} — race condition?"
        )));
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

    let registry = pools.registry_pool();
    let existing: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch_pool(&registry)
        .await?;
    let Some(org) = existing.into_iter().next() else {
        return Err(TenancyError::Validation(format!(
            "purge-tenant: no tenant with slug `{slug}`"
        )));
    };

    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!("org `{slug}` has unknown storage_mode `{got}`"))
    })?;

    match mode {
        StorageMode::Schema => {
            // Schema mode is PG-only by language. Downcast to grab
            // the typed `pools.registry()` PgPool accessor.
            #[cfg(feature = "postgres")]
            {
                let pg_pools = (pools as &dyn std::any::Any)
                    .downcast_ref::<TenantPools<sqlx::Postgres>>()
                    .ok_or_else(|| {
                        TenancyError::Validation(
                            "purge-tenant: schema-mode tenants require a Postgres registry".into(),
                        )
                    })?;
                let schema = org.schema_name.clone().unwrap_or_else(|| slug.clone());
                let sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(&schema));
                rustango::sql::sqlx::query(&sql)
                    .execute(pg_pools.registry())
                    .await?;
                writeln!(w, "purged tenant `{slug}` (dropped schema `{schema}`)")?;
            }
            #[cfg(not(feature = "postgres"))]
            {
                return Err(TenancyError::Validation(
                    "purge-tenant: schema-mode tenants require the `postgres` feature".into(),
                ));
            }
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
            // v0.38 — DROP DATABASE is PG-only via this helper for
            // now. On sqlite the operator deletes the .db file out-
            // of-band; on mysql the syntax matches but the admin-
            // db rewrite is different so we'd need a parallel helper.
            #[cfg(feature = "postgres")]
            {
                drop_database_at(&url, w).await?;
            }
            #[cfg(not(feature = "postgres"))]
            {
                writeln!(
                    w,
                    "  purged tenant `{slug}` registry row; manually delete the \
                     tenant database at `{url}` (DROP DATABASE not wired for \
                     sqlite/mysql in v0.38)",
                )?;
            }
            writeln!(w, "purged tenant `{slug}` (dropped dedicated database)")?;
        }
    }

    // DELETE the Org row via the ORM's bi-dialect `delete_pool`.
    let id = org
        .id
        .get()
        .copied()
        .ok_or_else(|| TenancyError::Validation("purge-tenant: Org row has no PK".into()))?;
    let rows_deleted = org.delete_pool(&registry).await?;
    if rows_deleted == 0 {
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
/// v0.38 — PG-only. SQLite databases live in a single file (delete
/// the file); MySQL has `DROP DATABASE` but the per-tenant URL
/// rewrite gymnastics are different enough that lifting this helper
/// to be tri-dialect is queued separately.
#[cfg(feature = "postgres")]
async fn drop_database_at<W: Write + Send>(
    tenant_url: &str,
    w: &mut W,
) -> Result<(), TenancyError> {
    use crate::sql::sqlx::postgres::PgConnectOptions;
    use crate::sql::sqlx::ConnectOptions;
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
    if dbname.eq_ignore_ascii_case("postgres")
        || dbname.eq_ignore_ascii_case("template0")
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
    rustango::sql::sqlx::query(&sql).execute(&mut admin).await?;
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
    let orgs: Vec<Org> = Org::objects().fetch_pool(&pools.registry_pool()).await?;
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
