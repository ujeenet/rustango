//! User-account verbs: `create-operator` (registry-side, slice 6) and
//! `create-user` (per-tenant, slice 6).

use std::io::Write;

use crate::core::Column as _;
use crate::sql::{Auto, Fetcher};

use crate::tenancy::error::TenancyError;
use crate::tenancy::manage::args::{next_value, quote_ident};
use crate::tenancy::manage_interactive;
use crate::tenancy::pools::TenantPools;

// ---------- create-operator (Slice 6) ----------

pub(super) async fn create_operator_cmd<W: Write + Send>(
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
        None => manage_interactive::ask("Username: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "create-operator requires a username positional argument".into(),
                )
            })?,
    };
    let plain = match password {
        Some(p) => p,
        None => manage_interactive::ask_password("Password: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("create-operator requires --password".into())
            })?,
    };

    // Reject duplicate username up front.
    let existing: Vec<crate::tenancy::Operator> = crate::tenancy::Operator::objects()
        .where_(crate::tenancy::Operator::username.eq(username.clone()))
        .fetch(pools.registry())
        .await?;
    if !existing.is_empty() {
        return Err(TenancyError::Validation(format!(
            "operator `{username}` already exists in the registry"
        )));
    }

    let mut op = crate::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: crate::tenancy::password::hash(&plain)?,
        active: true,
        created_at: chrono::Utc::now(),
    };
    op.insert(pools.registry()).await?;
    let id = op.id.get().copied().unwrap_or_default();
    writeln!(w, "created operator `{username}` (id {id})")?;
    Ok(())
}

// ---------- create-user (Slice 6) ----------

pub(super) async fn create_user_cmd<W: Write + Send>(
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
        None => manage_interactive::ask("Tenant slug: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "create-user requires a tenant slug as the first positional argument".into(),
                )
            })?,
    };
    let username = match username_arg {
        Some(u) => u,
        None => manage_interactive::ask("Username: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "create-user requires a username as the second positional argument".into(),
                )
            })?,
    };
    let plain = match password {
        Some(p) => p,
        None => manage_interactive::ask_password("Password: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| TenancyError::Validation("create-user requires --password".into()))?,
    };

    // Look up the tenant.
    let orgs: Vec<crate::tenancy::Org> = crate::tenancy::Org::objects()
        .where_(crate::tenancy::Org::slug.eq(slug.clone()))
        .fetch(pools.registry())
        .await?;
    let org = orgs.into_iter().next().ok_or_else(|| {
        TenancyError::Validation(format!("create-user: no tenant with slug `{slug}`"))
    })?;

    let hash = crate::tenancy::password::hash(&plain)?;
    let now = chrono::Utc::now().to_rfc3339();

    // We bypass `User::insert` because that uses `pools.registry()`'s
    // pool by default and we need a connection scoped to the tenant.
    // Hand-write an INSERT against the scoped connection.
    use crate::sql::sqlx::Row;
    use crate::tenancy::org::StorageMode;
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!("org `{slug}` has unknown storage_mode `{got}`"))
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
    use crate::sql::sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;
    let schema_owned: Arc<str> = Arc::from(schema);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _meta| {
            let schema = Arc::clone(&schema_owned);
            Box::pin(async move {
                let stmt = format!("SET search_path TO {}, public", quote_ident(&schema));
                rustango::sql::sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect(registry_url)
        .await?;
    Ok(pool)
}
