//! User-account verbs: `create-operator` (registry-side, slice 6) and
//! `create-user` (per-tenant, slice 6).
//!
//! v0.38 — generic over the tenant backend via `TenantPools<DB>`.
//! The per-tenant verbs (create-user / set-superuser / reset-password)
//! rely on `scoped_tenant_pool` which on PG dispatches through schema-
//! mode + database-mode, and on sqlite/mysql resolves to database-mode
//! only. The `_pool` ORM family (`fetch`, `save_pool`,
//! `update_pool`) drives every backend-uniform query path so the same
//! verb body runs on PG / MySQL / SQLite.

use std::io::Write;

use sqlx::Database;

use crate::core::Column as _;
use crate::sql::{Auto, FetcherPool};

use crate::tenancy::error::TenancyError;
#[cfg(feature = "postgres")]
use crate::tenancy::manage::args::quote_ident;
use crate::tenancy::manage::args::{next_value, reject_leading_flag};
use crate::tenancy::manage_interactive;
use crate::tenancy::pools::TenantPools;

// ---------- create-operator (Slice 6) ----------

pub(super) async fn create_operator_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(
        args,
        "create-operator",
        "username",
        "create-operator <username> [--password <p> | --generate]",
    )?;
    let mut iter = args.iter();
    let username_arg = iter.next().cloned();
    let mut password: Option<String> = None;
    let mut generate = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--generate" => generate = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "create-operator <username> [--password <p> | --generate]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-operator: unknown argument `{other}`"
                )));
            }
        }
    }
    if generate && password.is_some() {
        return Err(TenancyError::Validation(
            "create-operator: --generate and --password are mutually exclusive".into(),
        ));
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
    let (plain, generated) = if generate {
        let p = crate::tenancy::password::generate(20);
        (p, true)
    } else {
        let p = match password {
            Some(p) => p,
            None => manage_interactive::ask_password("Password: ")
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation("create-operator requires --password".into())
                })?,
        };
        (p, false)
    };

    // Reject duplicate username up front.
    let registry = pools.registry_pool();
    let existing: Vec<crate::tenancy::Operator> = crate::tenancy::Operator::objects()
        .where_(crate::tenancy::Operator::username.eq(username.clone()))
        .fetch(&registry)
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
        password_changed_at: None,
    };
    op.insert_pool(&registry).await?;
    let id = op.id.get().copied().unwrap_or_default();
    if generated {
        writeln!(w, "created operator `{username}` (id {id})")?;
        writeln!(w, "  generated password: {plain}")?;
        writeln!(w, "  store this safely — it won't be shown again")?;
    } else {
        writeln!(w, "created operator `{username}` (id {id})")?;
    }
    Ok(())
}

// ---------- create-user (Slice 6) ----------

pub(super) async fn create_user_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(
        args,
        "create-user",
        "slug",
        "create-user <slug> <username> [--password <p> | --generate] [--superuser]",
    )?;
    let mut iter = args.iter();
    let slug_arg = iter.next().cloned();
    let username_arg = iter.next().cloned();
    let mut password: Option<String> = None;
    let mut generate = false;
    let mut is_superuser = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--generate" => generate = true,
            "--superuser" => is_superuser = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "create-user <slug> <username> [--password <p> | --generate] [--superuser]"
                        .into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "create-user: unknown argument `{other}`"
                )));
            }
        }
    }
    if generate && password.is_some() {
        return Err(TenancyError::Validation(
            "create-user: --generate and --password are mutually exclusive".into(),
        ));
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
    let (plain, generated) = if generate {
        (crate::tenancy::password::generate(20), true)
    } else {
        let p = match password {
            Some(p) => p,
            None => manage_interactive::ask_password("Password: ")
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation("create-user requires --password".into())
                })?,
        };
        (p, false)
    };

    // Look up the tenant.
    let registry = pools.registry_pool();
    let orgs: Vec<crate::tenancy::Org> = crate::tenancy::Org::objects()
        .where_(crate::tenancy::Org::slug.eq(slug.clone()))
        .fetch(&registry)
        .await?;
    orgs.into_iter().next().ok_or_else(|| {
        TenancyError::Validation(format!("create-user: no tenant with slug `{slug}`"))
    })?;

    let hash = crate::tenancy::password::hash(&plain)?;

    // v0.38 — open a tenant-scoped Pool enum (handles schema-mode on
    // PG, database-mode on any backend). Then drive the read/write
    // through the tri-dialect ORM (FetcherPool::fetch +
    // Model::save_pool) so the same code runs on PG / MySQL / SQLite.
    use crate::sql::FetcherPool as _;
    let scoped = scoped_tenant_pool(pools, registry_url, &slug).await?;

    // v0.27.6 — first-user-auto-superuser. When a tenant has zero
    // existing rows in `rustango_users`, the first user is implicitly
    // promoted to superuser even if `--superuser` wasn't passed.
    let mut auto_promoted = false;
    if !is_superuser {
        let existing: Vec<crate::tenancy::User> = crate::tenancy::User::objects()
            .fetch(&scoped)
            .await
            .unwrap_or_default();
        if existing.is_empty() {
            is_superuser = true;
            auto_promoted = true;
        }
    }

    let mut user = crate::tenancy::User {
        id: Auto::default(),
        username: username.clone(),
        password_hash: hash,
        email: None,
        is_superuser,
        active: true,
        created_at: chrono::Utc::now(),
        data: serde_json::Value::Object(serde_json::Map::new()),
        password_changed_at: None,
    };
    user.save_pool(&scoped).await?;
    let row_id: i64 = user.id.get().copied().unwrap_or_default();
    if auto_promoted {
        writeln!(
            w,
            "created user `{username}` in tenant `{slug}` (id {row_id}, superuser=true) — \
             auto-promoted because they're the first user of the tenant; pass `--superuser` \
             explicitly to silence this notice on subsequent setups"
        )?;
    } else {
        writeln!(
            w,
            "created user `{username}` in tenant `{slug}` (id {row_id}, superuser={is_superuser})"
        )?;
    }
    if generated {
        writeln!(w, "  generated password: {plain}")?;
        writeln!(w, "  store this safely — it won't be shown again")?;
    }
    Ok(())
}

// ---------- create-superuser (v0.27.6, #77 partial) ----------

/// Django-shape `create-superuser <slug> <username> [--password <s>]`.
/// Convenience entrypoint that always sets `is_superuser = true` —
/// equivalent to `create-user <slug> <username> --superuser` but
/// with a clearer name and prompts when args are missing.
pub(super) async fn create_superuser_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // Forward to `create_user_cmd` with `--superuser` injected.
    let mut forwarded: Vec<String> = args.to_vec();
    if !forwarded.iter().any(|s| s == "--superuser") {
        forwarded.push("--superuser".into());
    }
    create_user_cmd(pools, registry_url, &forwarded, w).await
}

// ---------- set-superuser (v0.27.6) ----------

/// `set-superuser <slug> <username> [--on|--off]` toggles
/// `rustango_users.is_superuser` on an existing tenant user. The
/// non-superuser-can't-see-anything failure mode is the most common
/// reason for this verb existing — a freshly-onboarded user lacks
/// any granted CRUD codenames and the admin sidebar appears empty.
/// Promoting them to superuser bypasses the per-codename check.
pub(super) async fn set_superuser_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(
        args,
        "set-superuser",
        "slug",
        "set-superuser <slug> <username> [--on|--off]",
    )?;
    let mut iter = args.iter();
    let slug = iter.next().cloned().ok_or_else(|| {
        TenancyError::Validation("set-superuser <slug> <username> [--on|--off]".into())
    })?;
    let username = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation("set-superuser requires a username".into()))?;
    let mut on = true;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--on" => on = true,
            "--off" => on = false,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "set-superuser <slug> <username> [--on|--off]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "set-superuser: unknown argument `{other}`"
                )));
            }
        }
    }
    let pool = scoped_tenant_pool(pools, registry_url, &slug).await?;
    // v0.38 — UPDATE via tri-dialect raw_execute_pool. Dialect
    // emitter picks placeholders ($1/$2 on PG, ? on sqlite/mysql).
    let dialect = pool.dialect();
    let users_t = dialect.quote_ident("rustango_users");
    let is_super_col = dialect.quote_ident("is_superuser");
    let username_col = dialect.quote_ident("username");
    let p1 = dialect.placeholder(1);
    let p2 = dialect.placeholder(2);
    let sql = format!("UPDATE {users_t} SET {is_super_col} = {p1} WHERE {username_col} = {p2}");
    let affected = rustango::sql::raw_execute_pool(
        &pool,
        &sql,
        vec![
            rustango::core::SqlValue::from(on),
            rustango::core::SqlValue::from(username.clone()),
        ],
    )
    .await
    .map_err(|e| TenancyError::Validation(format!("set-superuser: {e}")))?;
    if affected == 0 {
        return Err(TenancyError::Validation(format!(
            "set-superuser: no user `{username}` in tenant `{slug}`"
        )));
    }
    writeln!(
        w,
        "set is_superuser={on} on user `{username}` in tenant `{slug}`"
    )?;
    Ok(())
}

// ---------- reset-password (v0.27.6, #77 partial) ----------

/// `reset-password <slug> <username> [--password <s>]` updates a
/// tenant user's password hash without requiring the current
/// password. Use this from the admin's perspective to recover a
/// locked-out user; tenant users themselves should change their
/// password via the (still-pending #77) self-serve UI.
pub(super) async fn reset_password_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(
        args,
        "reset-password",
        "slug",
        "reset-password <slug> <username> [--password <s> | --generate]",
    )?;
    let mut iter = args.iter();
    let slug = iter.next().cloned().ok_or_else(|| {
        TenancyError::Validation(
            "reset-password <slug> <username> [--password <s> | --generate]".into(),
        )
    })?;
    let username = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation("reset-password requires a username".into()))?;
    let mut password: Option<String> = None;
    let mut generate = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--generate" => generate = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "reset-password <slug> <username> [--password <s> | --generate]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "reset-password: unknown argument `{other}`"
                )));
            }
        }
    }
    if generate && password.is_some() {
        return Err(TenancyError::Validation(
            "reset-password: --generate and --password are mutually exclusive".into(),
        ));
    }
    let (plain, generated) = if generate {
        (crate::tenancy::password::generate(20), true)
    } else {
        let p = match password {
            Some(p) => p,
            None => manage_interactive::ask_password("New password: ")
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation("reset-password requires --password (or a TTY)".into())
                })?,
        };
        (p, false)
    };
    let hash = crate::tenancy::password::hash(&plain)?;
    let pool = scoped_tenant_pool(pools, registry_url, &slug).await?;
    // v0.38 — tri-dialect UPDATE. Bind chrono::Utc::now() instead of
    // SQL `NOW()` so the same code works on PG/MySQL (NOW()) and
    // SQLite (CURRENT_TIMESTAMP). raw_execute_pool routes per dialect.
    let dialect = pool.dialect();
    let users_t = dialect.quote_ident("rustango_users");
    let hash_col = dialect.quote_ident("password_hash");
    let ts_col = dialect.quote_ident("password_changed_at");
    let username_col = dialect.quote_ident("username");
    let p1 = dialect.placeholder(1);
    let p2 = dialect.placeholder(2);
    let p3 = dialect.placeholder(3);
    let sql = format!(
        "UPDATE {users_t} SET {hash_col} = {p1}, {ts_col} = {p2} WHERE {username_col} = {p3}"
    );
    let affected = rustango::sql::raw_execute_pool(
        &pool,
        &sql,
        vec![
            rustango::core::SqlValue::from(hash.clone()),
            rustango::core::SqlValue::DateTime(chrono::Utc::now()),
            rustango::core::SqlValue::from(username.clone()),
        ],
    )
    .await
    .map_err(|e| TenancyError::Validation(format!("reset-password: {e}")))?;
    if affected == 0 {
        return Err(TenancyError::Validation(format!(
            "reset-password: no user `{username}` in tenant `{slug}`"
        )));
    }
    writeln!(w, "password reset for user `{username}` in tenant `{slug}`")?;
    if generated {
        writeln!(w, "  generated password: {plain}")?;
        writeln!(w, "  store this safely — it won't be shown again")?;
    }
    Ok(())
}

// ---------- reset-operator-password (v0.27.6) ----------

/// `reset-operator-password <username> [--password <s>]` updates an
/// operator's password hash on the registry pool. Recovery path
/// when an operator forgets their password and there's no other
/// admin who can reset via the (pending #77) UI.
pub(super) async fn reset_operator_password_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    reject_leading_flag(
        args,
        "reset-operator-password",
        "username",
        "reset-operator-password <username> [--password <s> | --generate]",
    )?;
    let mut iter = args.iter();
    let username = iter.next().cloned().ok_or_else(|| {
        TenancyError::Validation(
            "reset-operator-password <username> [--password <s> | --generate]".into(),
        )
    })?;
    let mut password: Option<String> = None;
    let mut generate = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--generate" => generate = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "reset-operator-password <username> [--password <s> | --generate]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "reset-operator-password: unknown argument `{other}`"
                )));
            }
        }
    }
    if generate && password.is_some() {
        return Err(TenancyError::Validation(
            "reset-operator-password: --generate and --password are mutually exclusive".into(),
        ));
    }
    let (plain, generated) = if generate {
        (crate::tenancy::password::generate(20), true)
    } else {
        let p = match password {
            Some(p) => p,
            None => manage_interactive::ask_password("New password: ")
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation(
                        "reset-operator-password requires --password (or a TTY)".into(),
                    )
                })?,
        };
        (p, false)
    };
    let hash = crate::tenancy::password::hash(&plain)?;
    // Route the operator-password rotate through the ORM so the SQL
    // gets per-dialect placeholders + identifier quoting + `NOW()` is
    // a value we set on the Rust side (chrono::Utc::now()) instead of
    // a PG-only SQL function.
    let registry = pools.registry_pool();
    let existing: Vec<crate::tenancy::Operator> = crate::tenancy::Operator::objects()
        .where_(crate::tenancy::Operator::username.eq(username.clone()))
        .fetch(&registry)
        .await?;
    let mut op = existing.into_iter().next().ok_or_else(|| {
        TenancyError::Validation(format!(
            "reset-operator-password: no operator named `{username}`"
        ))
    })?;
    op.password_hash = hash;
    op.password_changed_at = Some(chrono::Utc::now());
    op.save_pool(&registry).await?;
    writeln!(w, "password reset for operator `{username}`")?;
    if generated {
        writeln!(w, "  generated password: {plain}")?;
        writeln!(w, "  store this safely — it won't be shown again")?;
    }
    Ok(())
}

// ---------- change-password (v0.28.2, #77) ----------

/// `change-password <slug> <username>` rotates a tenant user's
/// password by first verifying the current password. Use this when
/// the user remembers their current password — it's the symmetric
/// CLI counterpart to the self-serve change-password UI. Operators
/// recovering a locked-out user should use `reset-password` instead.
///
/// Both passwords are read interactively from a TTY when not passed
/// on the command line; the prompts are echo-suppressed.
pub(super) async fn change_password_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut iter = args.iter();
    let slug = iter.next().cloned().ok_or_else(|| {
        TenancyError::Validation(
            "change-password <slug> <username> [--current <s>] [--password <s> | --generate]"
                .into(),
        )
    })?;
    let username = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation("change-password requires a username".into()))?;
    let mut current: Option<String> = None;
    let mut password: Option<String> = None;
    let mut generate = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--current" => current = Some(next_value(&mut iter, "--current")?),
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--generate" => generate = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "change-password <slug> <username> [--current <s>] [--password <s> | --generate]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "change-password: unknown argument `{other}`"
                )));
            }
        }
    }
    if generate && password.is_some() {
        return Err(TenancyError::Validation(
            "change-password: --generate and --password are mutually exclusive".into(),
        ));
    }
    let cur_plain = match current {
        Some(p) => p,
        None => manage_interactive::ask_password("Current password: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation("change-password requires --current (or a TTY)".into())
            })?,
    };
    let (new_plain, generated) = if generate {
        (crate::tenancy::password::generate(20), true)
    } else {
        let p = match password {
            Some(p) => p,
            None => manage_interactive::ask_password("New password: ")
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation(
                        "change-password requires --password (or a TTY)".into(),
                    )
                })?,
        };
        (p, false)
    };

    let pool = scoped_tenant_pool(pools, registry_url, &slug).await?;
    // v0.38 — read the existing user row via the tri-dialect ORM.
    use crate::sql::FetcherPool as _;
    let users: Vec<crate::tenancy::User> = crate::tenancy::User::objects()
        .where_(crate::tenancy::User::username.eq(username.clone()))
        .fetch(&pool)
        .await?;
    let Some(mut user) = users.into_iter().next() else {
        return Err(TenancyError::Validation(format!(
            "change-password: no user `{username}` in tenant `{slug}`"
        )));
    };
    if !crate::tenancy::password::verify(&cur_plain, &user.password_hash)? {
        return Err(TenancyError::Validation(
            "change-password: current password did not match".into(),
        ));
    }
    user.password_hash = crate::tenancy::password::hash(&new_plain)?;
    user.password_changed_at = Some(chrono::Utc::now());
    user.save_pool(&pool).await?;
    writeln!(
        w,
        "password changed for user `{username}` in tenant `{slug}`"
    )?;
    if generated {
        writeln!(w, "  generated password: {new_plain}")?;
        writeln!(w, "  store this safely — it won't be shown again")?;
    }
    Ok(())
}

// ---------- change-operator-password (v0.28.2, #77) ----------

/// `change-operator-password <username>` rotates an operator's
/// password by first verifying the current password. Symmetric
/// counterpart to `reset-operator-password` for the case where the
/// operator still remembers their current credentials.
pub(super) async fn change_operator_password_cmd<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut iter = args.iter();
    let username = iter.next().cloned().ok_or_else(|| {
        TenancyError::Validation(
            "change-operator-password <username> [--current <s>] [--password <s> | --generate]"
                .into(),
        )
    })?;
    let mut current: Option<String> = None;
    let mut password: Option<String> = None;
    let mut generate = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--current" => current = Some(next_value(&mut iter, "--current")?),
            "--password" => password = Some(next_value(&mut iter, "--password")?),
            "--generate" => generate = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "change-operator-password <username> [--current <s>] [--password <s> | --generate]".into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "change-operator-password: unknown argument `{other}`"
                )));
            }
        }
    }
    if generate && password.is_some() {
        return Err(TenancyError::Validation(
            "change-operator-password: --generate and --password are mutually exclusive".into(),
        ));
    }
    let cur_plain = match current {
        Some(p) => p,
        None => manage_interactive::ask_password("Current password: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| {
                TenancyError::Validation(
                    "change-operator-password requires --current (or a TTY)".into(),
                )
            })?,
    };
    let (new_plain, generated) = if generate {
        (crate::tenancy::password::generate(20), true)
    } else {
        let p = match password {
            Some(p) => p,
            None => manage_interactive::ask_password("New password: ")
                .map_err(TenancyError::Io)?
                .ok_or_else(|| {
                    TenancyError::Validation(
                        "change-operator-password requires --password (or a TTY)".into(),
                    )
                })?,
        };
        (p, false)
    };

    // ORM lookup + save path so the change-password verb works on any
    // backend the registry runs on.
    let registry = pools.registry_pool();
    let existing: Vec<crate::tenancy::Operator> = crate::tenancy::Operator::objects()
        .where_(crate::tenancy::Operator::username.eq(username.clone()))
        .fetch(&registry)
        .await?;
    let mut op = existing.into_iter().next().ok_or_else(|| {
        TenancyError::Validation(format!(
            "change-operator-password: no operator named `{username}`"
        ))
    })?;
    if !crate::tenancy::password::verify(&cur_plain, &op.password_hash)? {
        return Err(TenancyError::Validation(
            "change-operator-password: current password did not match".into(),
        ));
    }
    op.password_hash = crate::tenancy::password::hash(&new_plain)?;
    op.password_changed_at = Some(chrono::Utc::now());
    op.save_pool(&registry).await?;
    writeln!(w, "password changed for operator `{username}`")?;
    if generated {
        writeln!(w, "  generated password: {new_plain}")?;
        writeln!(w, "  store this safely — it won't be shown again")?;
    }
    Ok(())
}

/// Open a short-lived `PgPool` scoped to `slug`'s tenant — schema
/// mode pre-sets `search_path`; database mode reuses the cached
/// per-tenant pool. Shared by `set-superuser` / `reset-password`.
pub(super) async fn scoped_tenant_pool<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    slug: &str,
) -> Result<rustango::sql::Pool, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let orgs: Vec<crate::tenancy::Org> = crate::tenancy::Org::objects()
        .where_(crate::tenancy::Org::slug.eq(slug.to_owned()))
        .fetch(&pools.registry_pool())
        .await?;
    let org = orgs
        .into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("no tenant with slug `{slug}`")))?;
    use crate::tenancy::org::StorageMode;
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!("org `{slug}` has unknown storage_mode `{got}`"))
    })?;
    match mode {
        StorageMode::Schema => {
            #[cfg(feature = "postgres")]
            {
                let schema = org.schema_name.unwrap_or_else(|| slug.to_owned());
                let pg = build_schema_scoped_pool(registry_url, &schema).await?;
                Ok(rustango::sql::Pool::Postgres(pg))
            }
            #[cfg(not(feature = "postgres"))]
            {
                let _ = registry_url;
                Err(TenancyError::Validation(format!(
                    "tenant `{slug}` is schema-mode but `postgres` feature is off"
                )))
            }
        }
        StorageMode::Database => {
            let tp = pools.database_pool_for_org(&org).await?;
            match tp {
                crate::tenancy::TenantPool::Database { pool } => {
                    Ok(rustango::sql::Pool::from((*pool).clone()))
                }
                #[cfg(feature = "postgres")]
                crate::tenancy::TenantPool::Schema { .. } => {
                    unreachable!("database_pool_for_org rejects schema-mode")
                }
            }
        }
    }
}

/// Mirror of the migration helper — build a short-lived pool whose
/// connections have `search_path` pre-set. Local copy so manage
/// doesn't need a public reference into [`crate::migrate`].
/// PG-only by language.
#[cfg(feature = "postgres")]
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
