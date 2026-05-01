//! `create-role`, `assign-role`, `revoke-role`, `list-roles`,
//! `grant-perm`, `revoke-perm`, `create-api-key` manage verbs.

use std::io::Write;

use crate::core::Column as _;
use crate::sql::Fetcher as _;
use crate::tenancy::{auth_backends, permissions, Org, User};

use crate::sql::sqlx::{PgPool, Row};

use super::super::error::TenancyError;
use super::super::pools::TenantPools;
use super::args::next_value;

// ------------------------------------------------------------------ create-role

pub(super) async fn create_role_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut iter = args.iter();
    let slug = next_value(&mut iter, "<tenant-slug>")?;
    let name = next_value(&mut iter, "<role-name>")?;
    let mut description = String::new();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--description" => description = next_value(&mut iter, "--description")?,
            "--help" | "-h" => {
                writeln!(w, "create-role <slug> <name> [--description <s>]")?;
                return Ok(());
            }
            other => {
                return Err(TenancyError::Validation(format!("unknown flag `{other}`")))
            }
        }
    }
    let pool = tenant_pool_for_slug(pools, &slug).await?;
    let id = permissions::create_role(&name, &description, &pool)
        .await
        .map_err(TenancyError::Driver)?;
    writeln!(w, "created role `{name}` (id={id}) on tenant `{slug}`")?;
    Ok(())
}

// ------------------------------------------------------------------ list-roles

pub(super) async fn list_roles_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut iter = args.iter();
    let slug = next_value(&mut iter, "<tenant-slug>")?;
    let pool = tenant_pool_for_slug(pools, &slug).await?;
    let rows = crate::sql::sqlx::query(
        r#"SELECT r.id, r.name, r.description,
                  COUNT(rp.codename) AS perm_count
           FROM "rustango_roles" r
           LEFT JOIN "rustango_role_permissions" rp ON rp.role_id = r.id
           GROUP BY r.id
           ORDER BY r.name"#,
    )
    .fetch_all(&pool)
    .await
    .map_err(TenancyError::Driver)?;

    if rows.is_empty() {
        writeln!(w, "(no roles on tenant `{slug}`)")?;
        return Ok(());
    }
    writeln!(w, "{:<6} {:<30} {:<8} description", "id", "name", "perms")?;
    writeln!(w, "{}", "-".repeat(60))?;
    for row in &rows {
        let id: i64 = row.try_get("id").unwrap_or(0);
        let name: String = row.try_get("name").unwrap_or_default();
        let desc: String = row.try_get("description").unwrap_or_default();
        let count: i64 = row.try_get("perm_count").unwrap_or(0);
        writeln!(w, "{id:<6} {name:<30} {count:<8} {desc}")?;
    }
    Ok(())
}

// ------------------------------------------------------------------ assign-role / revoke-role

pub(super) async fn assign_role_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    role_membership_cmd(pools, args, w, true).await
}

pub(super) async fn revoke_role_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    role_membership_cmd(pools, args, w, false).await
}

async fn role_membership_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
    assign: bool,
) -> Result<(), TenancyError> {
    let verb = if assign { "assign-role" } else { "revoke-role" };
    let mut iter = args.iter();
    let slug = next_value(&mut iter, "<tenant-slug>")?;
    let username = next_value(&mut iter, "<username>")?;
    let role_name = next_value(&mut iter, "<role-name>")?;

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    let user_id = user_id_by_username(&username, &pool).await?;
    let role_id = role_id_by_name(&role_name, &pool).await?;

    if assign {
        permissions::assign_role(user_id, role_id, &pool)
            .await
            .map_err(TenancyError::Driver)?;
        writeln!(w, "assigned role `{role_name}` to `{username}` on tenant `{slug}`")?;
    } else {
        permissions::remove_role(user_id, role_id, &pool)
            .await
            .map_err(TenancyError::Driver)?;
        writeln!(w, "removed role `{role_name}` from `{username}` on tenant `{slug}`")?;
    }
    Ok(())
}

// ------------------------------------------------------------------ grant-perm / revoke-perm

pub(super) async fn grant_perm_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut iter = args.iter();
    let slug = next_value(&mut iter, "<tenant-slug>")?;
    let target = next_value(&mut iter, "<role-name|username>")?;
    let codename = next_value(&mut iter, "<codename>")?;
    let mut to_role = false;
    while let Some(flag) = iter.next() {
        if flag == "--role" { to_role = true; }
    }

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    if to_role {
        let role_id = role_id_by_name(&target, &pool).await?;
        permissions::grant_role_perm(role_id, &codename, &pool)
            .await
            .map_err(TenancyError::Driver)?;
        writeln!(w, "granted `{codename}` to role `{target}` on tenant `{slug}`")?;
    } else {
        let user_id = user_id_by_username(&target, &pool).await?;
        permissions::set_user_perm(user_id, &codename, true, &pool)
            .await
            .map_err(TenancyError::Driver)?;
        writeln!(w, "granted `{codename}` to user `{target}` on tenant `{slug}`")?;
    }
    Ok(())
}

pub(super) async fn revoke_perm_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut iter = args.iter();
    let slug = next_value(&mut iter, "<tenant-slug>")?;
    let target = next_value(&mut iter, "<role-name|username>")?;
    let codename = next_value(&mut iter, "<codename>")?;
    let mut to_role = false;
    while let Some(flag) = iter.next() {
        if flag == "--role" { to_role = true; }
    }

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    if to_role {
        let role_id = role_id_by_name(&target, &pool).await?;
        permissions::revoke_role_perm(role_id, &codename, &pool)
            .await
            .map_err(TenancyError::Driver)?;
        writeln!(w, "revoked `{codename}` from role `{target}` on tenant `{slug}`")?;
    } else {
        let user_id = user_id_by_username(&target, &pool).await?;
        permissions::set_user_perm(user_id, &codename, false, &pool)
            .await
            .map_err(TenancyError::Driver)?;
        writeln!(w, "denied `{codename}` for user `{target}` on tenant `{slug}`")?;
    }
    Ok(())
}

// ------------------------------------------------------------------ create-api-key

pub(super) async fn create_api_key_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
    let mut iter = args.iter();
    let slug = next_value(&mut iter, "<tenant-slug>")?;
    let username = next_value(&mut iter, "<username>")?;
    let mut label = String::new();
    let mut expires_days: Option<i64> = None;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--label" => label = next_value(&mut iter, "--label")?,
            "--expires-days" => {
                let raw = next_value(&mut iter, "--expires-days")?;
                expires_days = Some(raw.parse::<i64>().map_err(|_| {
                    TenancyError::Validation(format!("--expires-days expects an integer, got `{raw}`"))
                })?);
            }
            "--help" | "-h" => {
                writeln!(w, "create-api-key <slug> <username> [--label <s>] [--expires-days <N>]")?;
                return Ok(());
            }
            other => return Err(TenancyError::Validation(format!("unknown flag `{other}`"))),
        }
    }

    let pool = tenant_pool_for_slug(pools, &slug).await?;
    auth_backends::ensure_api_keys_table(&pool)
        .await
        .map_err(TenancyError::Driver)?;
    let user_id = user_id_by_username(&username, &pool).await?;
    let expires_at = expires_days.map(|d| chrono::Utc::now() + chrono::Duration::days(d));
    let token = auth_backends::create_api_key(user_id, &label, expires_at, &pool)
        .await
        .map_err(TenancyError::Driver)?;

    writeln!(w, "API key for `{username}` on tenant `{slug}`:")?;
    writeln!(w, "  {token}")?;
    writeln!(w, "Store this — it won't be shown again.")?;
    Ok(())
}

// ------------------------------------------------------------------ helpers

async fn tenant_pool_for_slug(pools: &TenantPools, slug: &str) -> Result<PgPool, TenancyError> {
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pools.registry())
        .await?;
    let org = orgs.into_iter().next().ok_or_else(|| {
        TenancyError::Validation(format!("tenant `{slug}` not found"))
    })?;
    let tenant_pool = pools.pool_for_org(&org).await?;
    Ok(tenant_pool.pool().clone())
}

async fn user_id_by_username(username: &str, pool: &PgPool) -> Result<i64, TenancyError> {
    let row = crate::sql::sqlx::query(
        r#"SELECT id FROM "rustango_users" WHERE username = $1"#,
    )
    .bind(username)
    .fetch_optional(pool)
    .await
    .map_err(TenancyError::Driver)?;
    row.ok_or_else(|| TenancyError::Validation(format!("user `{username}` not found")))?
        .try_get("id")
        .map_err(TenancyError::Driver)
}

async fn role_id_by_name(name: &str, pool: &PgPool) -> Result<i64, TenancyError> {
    let row = crate::sql::sqlx::query(
        r#"SELECT id FROM "rustango_roles" WHERE name = $1"#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(TenancyError::Driver)?;
    row.ok_or_else(|| TenancyError::Validation(format!("role `{name}` not found")))?
        .try_get("id")
        .map_err(TenancyError::Driver)
}
