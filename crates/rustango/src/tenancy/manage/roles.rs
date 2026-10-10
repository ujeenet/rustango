//! `create-role`, `assign-role`, `revoke-role`, `list-roles`,
//! `grant-perm`, `revoke-perm`, `create-api-key` manage verbs.
//!
//! v0.38 — fully tri-dialect via `TenantPools<DB>` generics +
//! `_pool` ORM helpers + `scoped_pool_dyn` for schema-mode-aware
//! pool resolution.

use std::io::Write;

use sqlx::Database;

use crate::core::Column as _;
use crate::sql::FetcherPool as _;
use crate::tenancy::{auth_backends, permissions, Org, User};

use super::super::error::TenancyError;
use super::super::pools::TenantPools;
use super::args::{next_value, parse, Spec};

// ------------------------------------------------------------------ create-role

pub(super) async fn create_role_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    const USAGE: &str = "create-role <slug> <name> [--description <s>]";
    let parsed = parse(
        args,
        &Spec {
            verb: "create-role",
            usage: USAGE,
            switches: &[],
            valued: &["--description"],
            max_positionals: 2,
        },
    )?;
    let slug = parsed.required(0, USAGE)?;
    let name = parsed.required(1, USAGE)?;
    let description = parsed
        .value("--description")?
        .unwrap_or_default()
        .to_owned();
    let pool = tenant_pool_for_slug(pools, &slug).await?;
    let id = permissions::create_role_pool(&name, &description, &pool).await?;
    writeln!(w, "created role `{name}` (id={id}) on tenant `{slug}`")?;
    Ok(())
}

// ------------------------------------------------------------------ list-roles

pub(super) async fn list_roles_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    const USAGE: &str = "list-roles <slug>";
    let slug = parse(
        args,
        &Spec {
            verb: "list-roles",
            usage: USAGE,
            switches: &[],
            valued: &[],
            max_positionals: 1,
        },
    )?
    .required(0, USAGE)?;
    let pool = tenant_pool_for_slug(pools, &slug).await?;
    // v0.38 — list via ORM + per-role count via separate fetches.
    // Trades one JOIN-with-GROUP-BY query for N+1 queries; for the
    // tiny per-tenant role count (usually < 10) the perf delta is
    // unnoticeable, and the code stays tri-dialect without per-
    // dialect SQL.
    use crate::tenancy::permissions::{Role, RolePermission};
    let roles: Vec<Role> = Role::objects().fetch(&pool).await?;
    if roles.is_empty() {
        writeln!(w, "(no roles on tenant `{slug}`)")?;
        return Ok(());
    }
    writeln!(w, "{:<6} {:<30} {:<8} description", "id", "name", "perms")?;
    writeln!(w, "{}", "-".repeat(60))?;
    for role in &roles {
        let id = role.id.get().copied().unwrap_or(0);
        let perms: Vec<RolePermission> = RolePermission::objects()
            .where_(RolePermission::role_id.eq(id))
            .fetch(&pool)
            .await?;
        writeln!(
            w,
            "{id:<6} {:<30} {:<8} {}",
            role.name,
            perms.len(),
            role.description,
        )?;
    }
    Ok(())
}

// ------------------------------------------------------------------ assign-role / revoke-role

pub(super) async fn assign_role_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    role_membership_cmd(pools, args, w, true).await
}

pub(super) async fn revoke_role_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    role_membership_cmd(pools, args, w, false).await
}

async fn role_membership_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
    assign: bool,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let verb = if assign { "assign-role" } else { "revoke-role" };
    let usage = if assign {
        "assign-role <slug> <username> <role-name>"
    } else {
        "revoke-role <slug> <username> <role-name>"
    };
    // Trailing arguments were once ignored (#1952).
    let parsed = parse(
        args,
        &Spec {
            verb,
            usage,
            switches: &[],
            valued: &[],
            max_positionals: 3,
        },
    )?;
    let slug = parsed.required(0, usage)?;
    let username = parsed.required(1, usage)?;
    let role_name = parsed.required(2, usage)?;

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    let user_id = user_id_by_username(&username, &pool).await?;
    let role_id = role_id_by_name(&role_name, &pool).await?;

    if assign {
        permissions::assign_role_pool(user_id, role_id, &pool).await?;
        writeln!(
            w,
            "assigned role `{role_name}` to `{username}` on tenant `{slug}`"
        )?;
    } else {
        permissions::remove_role_pool(user_id, role_id, &pool).await?;
        writeln!(
            w,
            "removed role `{role_name}` from `{username}` on tenant `{slug}`"
        )?;
    }
    Ok(())
}

// ------------------------------------------------------------------ grant-perm / revoke-perm

/// `<slug> <target> <codename> [--role]`. A typo'd `--rol` used to be
/// ignored and grant to a *user* of that name (#1910).
fn perm_args(args: &[String], verb: &str) -> Result<(String, String, String, bool), TenancyError> {
    let usage = format!("{verb} <slug> <role-name|username> <codename> [--role]");
    let parsed = parse(
        args,
        &Spec {
            verb,
            usage: &usage,
            switches: &["--role"],
            valued: &[],
            max_positionals: 3,
        },
    )?;
    Ok((
        parsed.required(0, &usage)?,
        parsed.required(1, &usage)?,
        parsed.required(2, &usage)?,
        parsed.has("--role"),
    ))
}

pub(super) async fn grant_perm_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, target, codename, to_role) = perm_args(args, "grant-perm")?;

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    if to_role {
        let role_id = role_id_by_name(&target, &pool).await?;
        permissions::grant_role_perm_pool(role_id, &codename, &pool).await?;
        writeln!(
            w,
            "granted `{codename}` to role `{target}` on tenant `{slug}`"
        )?;
    } else {
        let user_id = user_id_by_username(&target, &pool).await?;
        permissions::set_user_perm_pool(user_id, &codename, true, &pool).await?;
        writeln!(
            w,
            "granted `{codename}` to user `{target}` on tenant `{slug}`"
        )?;
    }
    Ok(())
}

pub(super) async fn revoke_perm_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, target, codename, to_role) = perm_args(args, "revoke-perm")?;

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    if to_role {
        let role_id = role_id_by_name(&target, &pool).await?;
        permissions::revoke_role_perm_pool(role_id, &codename, &pool).await?;
        writeln!(
            w,
            "revoked `{codename}` from role `{target}` on tenant `{slug}`"
        )?;
    } else {
        let user_id = user_id_by_username(&target, &pool).await?;
        permissions::set_user_perm_pool(user_id, &codename, false, &pool).await?;
        writeln!(
            w,
            "denied `{codename}` for user `{target}` on tenant `{slug}`"
        )?;
    }
    Ok(())
}

// ------------------------------------------------------------------ create-api-key

pub(super) async fn create_api_key_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    const USAGE: &str = "create-api-key <slug> <username> [--label <s>] [--expires-days <N>]";
    let parsed = parse(
        args,
        &Spec {
            verb: "create-api-key",
            usage: USAGE,
            switches: &[],
            valued: &["--label", "--expires-days"],
            max_positionals: 2,
        },
    )?;
    let slug = parsed.required(0, USAGE)?;
    let username = parsed.required(1, USAGE)?;
    let label = parsed.value("--label")?.unwrap_or_default().to_owned();
    let expires_at = parsed
        .value("--expires-days")?
        .map(|raw| {
            // Checked: a huge count used to panic in chrono (#2395).
            raw.parse::<i64>()
                .ok()
                .filter(|d| *d > 0)
                .and_then(chrono::Duration::try_days)
                .and_then(|d| chrono::Utc::now().checked_add_signed(d))
                .ok_or_else(|| {
                    TenancyError::Validation(format!(
                        "--expires-days expects a positive number of days, got `{raw}`"
                    ))
                })
        })
        .transpose()?;

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    auth_backends::ensure_api_keys_table_pool(&pool)
        .await
        .map_err(TenancyError::Driver)?;
    let user_id = user_id_by_username(&username, &pool).await?;
    let token = auth_backends::create_api_key(user_id, &label, expires_at, &pool).await?;

    writeln!(w, "API key for `{username}` on tenant `{slug}`:")?;
    writeln!(w, "  {token}")?;
    writeln!(w, "Store this — it won't be shown again.")?;
    Ok(())
}

// ------------------------------------------------------------------ seed-permissions

pub(super) async fn seed_permissions_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut slug: Option<String> = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--slug" => slug = Some(next_value(&mut iter, "--slug")?),
            "--help" | "-h" => {
                writeln!(
                    w,
                    "seed-permissions [--slug <s>]\n  \
                     Re-run auto_create_permissions for one (with --slug) or every\n  \
                     active tenant, and ensure the permission and API-key tables.\n  \
                     Idempotent — UNIQUE on (table_name, codename)\n  \
                     means re-running on a populated catalog is a no-op."
                )?;
                return Ok(());
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "seed-permissions: unknown flag `{other}`"
                )));
            }
        }
    }

    let registry = pools.registry_pool();
    let targets: Vec<Org> = if let Some(s) = slug.as_deref() {
        let orgs: Vec<Org> = Org::objects()
            .where_(Org::slug.eq(s.to_owned()))
            .fetch(&registry)
            .await?;
        if orgs.is_empty() {
            return Err(TenancyError::Validation(format!("tenant `{s}` not found")));
        }
        orgs
    } else {
        Org::objects()
            .where_(Org::active.eq(true))
            .order_by(&[("slug", false)])
            .fetch(&registry)
            .await?
    };

    if targets.is_empty() {
        writeln!(w, "no active tenants to seed permissions for")?;
        return Ok(());
    }

    // One broken tenant must not stop the rest (#2156).
    let mut failures = 0;
    for org in &targets {
        match seed_one(pools, org).await {
            Ok(()) => writeln!(w, "seeded `{}`", org.slug)?,
            Err(e) => {
                failures += 1;
                writeln!(w, "failed `{}`: {e}", org.slug)?;
            }
        }
    }
    writeln!(w, "done — {} tenant(s) processed", targets.len())?;
    super::migrations::tenant_failures(failures, targets.len())
}

async fn seed_one<DB: Database>(pools: &TenantPools<DB>, org: &Org) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let pool = pools.scoped_pool_dyn(org).await?;
    permissions::ensure_tables_pool(&pool)
        .await
        .map_err(TenancyError::Driver)?;
    // The one path that recreates `rustango_api_keys` or its FK without minting a key (#1731).
    auth_backends::ensure_api_keys_table_pool(&pool)
        .await
        .map_err(TenancyError::Driver)?;
    permissions::auto_create_permissions_pool(&pool).await?;
    Ok(())
}

// ------------------------------------------------------------------ helpers

async fn tenant_pool_for_slug<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
) -> Result<crate::sql::Pool, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(&pools.registry_pool())
        .await?;
    let org = orgs
        .into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` not found")))?;
    pools.scoped_pool_dyn(&org).await
}

async fn user_id_by_username(username: &str, pool: &crate::sql::Pool) -> Result<i64, TenancyError> {
    let rows = User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch(pool)
        .await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("user `{username}` not found")))
        .map(|u| u.id.get().copied().unwrap_or(0))
}

async fn role_id_by_name(name: &str, pool: &crate::sql::Pool) -> Result<i64, TenancyError> {
    use crate::tenancy::permissions::Role;
    let rows = Role::objects()
        .where_(Role::name.eq(name.to_owned()))
        .fetch(pool)
        .await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("role `{name}` not found")))
        .map(|r| r.id.get().copied().unwrap_or(0))
}
