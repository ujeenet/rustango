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
        password_changed_at: None,
    };
    op.insert(pools.registry()).await?;
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
    // v0.27.6 — first-user-auto-superuser. When a tenant has zero
    // existing rows in `rustango_users`, the first user we create
    // is implicitly promoted to superuser even if `--superuser`
    // wasn't passed. Pre-fix, an onboarding script that ran
    // `create-user osu admin --password ...` (forgetting
    // `--superuser`) produced a tenant with exactly one user who
    // could log in but saw an empty admin sidebar (no granted
    // CRUD codenames → `is_visible()` returns false for every
    // table). Mirrors Django's `createsuperuser` first-user UX.
    // Caller can still pass `--superuser` for explicit intent;
    // future flag `--no-auto-superuser` could opt out.
    let mut auto_promoted = false;
    if !is_superuser {
        let existing_count: Option<i64> = match mode {
            StorageMode::Schema => {
                let schema = org.schema_name.clone().unwrap_or_else(|| slug.clone());
                let pool = build_schema_scoped_pool(registry_url, &schema).await?;
                let row = rustango::sql::sqlx::query("SELECT COUNT(*) AS n FROM rustango_users")
                    .fetch_one(&pool)
                    .await
                    .ok();
                pool.close().await;
                row.and_then(|r| r.try_get::<i64, _>("n").ok())
            }
            StorageMode::Database => {
                let tp = pools.pool_for_org(&org).await?;
                rustango::sql::sqlx::query("SELECT COUNT(*) AS n FROM rustango_users")
                    .fetch_one(tp.pool())
                    .await
                    .ok()
                    .and_then(|r| r.try_get::<i64, _>("n").ok())
            }
        };
        if existing_count == Some(0) {
            is_superuser = true;
            auto_promoted = true;
        }
    }
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
pub(super) async fn create_superuser_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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
pub(super) async fn set_superuser_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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
    let result = rustango::sql::sqlx::query(
        "UPDATE rustango_users SET is_superuser = $1 WHERE username = $2",
    )
    .bind(on)
    .bind(&username)
    .execute(&pool)
    .await?;
    pool.close().await;
    if result.rows_affected() == 0 {
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
pub(super) async fn reset_password_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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
    let result = rustango::sql::sqlx::query(
        "UPDATE rustango_users SET password_hash = $1, password_changed_at = NOW() WHERE username = $2",
    )
    .bind(&hash)
    .bind(&username)
    .execute(&pool)
    .await?;
    pool.close().await;
    if result.rows_affected() == 0 {
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
pub(super) async fn reset_operator_password_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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
    let result = rustango::sql::sqlx::query(
        "UPDATE rustango_operators SET password_hash = $1, password_changed_at = NOW() WHERE username = $2",
    )
    .bind(&hash)
    .bind(&username)
    .execute(pools.registry())
    .await?;
    if result.rows_affected() == 0 {
        return Err(TenancyError::Validation(format!(
            "reset-operator-password: no operator named `{username}`"
        )));
    }
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
pub(super) async fn change_password_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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
    let stored: Option<String> = rustango::sql::sqlx::query_scalar(
        "SELECT password_hash FROM rustango_users WHERE username = $1",
    )
    .bind(&username)
    .fetch_optional(&pool)
    .await?;
    let Some(stored_hash) = stored else {
        pool.close().await;
        return Err(TenancyError::Validation(format!(
            "change-password: no user `{username}` in tenant `{slug}`"
        )));
    };
    if !crate::tenancy::password::verify(&cur_plain, &stored_hash)? {
        pool.close().await;
        return Err(TenancyError::Validation(
            "change-password: current password did not match".into(),
        ));
    }
    let new_hash = crate::tenancy::password::hash(&new_plain)?;
    rustango::sql::sqlx::query("UPDATE rustango_users SET password_hash = $1, password_changed_at = NOW() WHERE username = $2")
        .bind(&new_hash)
        .bind(&username)
        .execute(&pool)
        .await?;
    pool.close().await;
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
pub(super) async fn change_operator_password_cmd<W: Write + Send>(
    pools: &TenantPools,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError> {
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

    let stored: Option<String> = rustango::sql::sqlx::query_scalar(
        "SELECT password_hash FROM rustango_operators WHERE username = $1",
    )
    .bind(&username)
    .fetch_optional(pools.registry())
    .await?;
    let Some(stored_hash) = stored else {
        return Err(TenancyError::Validation(format!(
            "change-operator-password: no operator named `{username}`"
        )));
    };
    if !crate::tenancy::password::verify(&cur_plain, &stored_hash)? {
        return Err(TenancyError::Validation(
            "change-operator-password: current password did not match".into(),
        ));
    }
    let new_hash = crate::tenancy::password::hash(&new_plain)?;
    rustango::sql::sqlx::query(
        "UPDATE rustango_operators SET password_hash = $1, password_changed_at = NOW() WHERE username = $2",
    )
    .bind(&new_hash)
    .bind(&username)
    .execute(pools.registry())
    .await?;
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
async fn scoped_tenant_pool(
    pools: &TenantPools,
    registry_url: &str,
    slug: &str,
) -> Result<rustango::sql::sqlx::PgPool, TenancyError> {
    let orgs: Vec<crate::tenancy::Org> = crate::tenancy::Org::objects()
        .where_(crate::tenancy::Org::slug.eq(slug.to_owned()))
        .fetch(pools.registry())
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
            let schema = org.schema_name.unwrap_or_else(|| slug.to_owned());
            build_schema_scoped_pool(registry_url, &schema).await
        }
        StorageMode::Database => {
            // Database-mode tenants use the cached pool — clone it
            // so the caller can `.close()` without affecting siblings.
            // Actually the cached pool is shared, so don't close;
            // clone the inner Arc<PgPool> handle instead.
            let tp = pools.pool_for_org(&org).await?;
            Ok(tp.pool().clone())
        }
    }
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
