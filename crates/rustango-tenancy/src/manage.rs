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
//! Hard-delete with schema/DB drop is intentionally **not** in slice 5 —
//! footgun-level operation. v0.6 ships `purge-tenant <slug>
//! --confirm <slug>` that requires typing the slug to drop schema or
//! call `DROP DATABASE`.

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
        "list-tenants" => list_tenants(pools, writer).await,
        "migrate-tenants" => migrate_tenants_cmd(pools, registry_url, dir, writer).await,
        "create-operator" => create_operator_cmd(pools, &args[1..], writer).await,
        "create-user" => create_user_cmd(pools, registry_url, &args[1..], writer).await,
        // Default to the single-tenant manage::run against the
        // registry pool. This handles `migrate`, `makemigrations`,
        // `downgrade`, `showmigrations`, `--help`, etc.
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
    let slug = iter
        .next()
        .ok_or_else(|| TenancyError::Validation("create-tenant requires a slug positional argument".into()))?
        .clone();
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
    let slug = iter
        .next()
        .ok_or_else(|| {
            TenancyError::Validation("drop-tenant requires a slug positional argument".into())
        })?
        .clone();
    let mut confirm: Option<String> = None;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--confirm" => {
                confirm = Some(next_value(&mut iter, "--confirm")?);
            }
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "drop-tenant <slug> --confirm <slug>\n  \
                     Soft-delete: sets active=false. Data is preserved.\n  \
                     `--confirm` must repeat the slug verbatim — guard against typos.".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "drop-tenant: unknown argument `{other}`"
                )));
            }
        }
    }
    let confirm = confirm.ok_or_else(|| {
        TenancyError::Validation(format!(
            "drop-tenant requires `--confirm {slug}` (repeat the slug verbatim)"
        ))
    })?;
    if confirm != slug {
        return Err(TenancyError::Validation(format!(
            "drop-tenant: --confirm value `{confirm}` does not match slug `{slug}`"
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
        "  to hard-delete (drop schema or DB), use `purge-tenant` (v0.6+)."
    )?;
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
    let username = iter
        .next()
        .ok_or_else(|| {
            TenancyError::Validation(
                "create-operator requires a username positional argument".into(),
            )
        })?
        .clone();
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
    let plain = password.ok_or_else(|| {
        TenancyError::Validation("create-operator requires --password".into())
    })?;

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
    let slug = iter
        .next()
        .ok_or_else(|| {
            TenancyError::Validation(
                "create-user requires a tenant slug as the first positional argument".into(),
            )
        })?
        .clone();
    let username = iter
        .next()
        .ok_or_else(|| {
            TenancyError::Validation(
                "create-user requires a username as the second positional argument".into(),
            )
        })?
        .clone();
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
    let plain = password.ok_or_else(|| {
        TenancyError::Validation("create-user requires --password".into())
    })?;

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
