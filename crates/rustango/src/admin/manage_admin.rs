//! The admin-user CLI verbs for a project that uses
//! `admin::Builder::with_session_auth`: `create-admin`, `reset-password`,
//! `change-password`, `set-admin-active` and `reset-admin-totp`.
//!
//! It takes a plain `&Pool`, with no tenancy resolver, writes to
//! `rustango_admin_users` (creating the table if needed), and hashes
//! the password with `crate::passwords::hash_async`.
//!
//! Usage:
//!
//! ```text
//! cargo run -- create-admin alice                          # prompts for password
//! cargo run -- create-admin alice --password-stdin         # scripts
//! cargo run -- create-admin alice --generate               # mints a random one + prints
//! cargo run -- create-admin alice --generate --superuser
//! ```

use std::io::Write;

use crate::core::Model;
use crate::manage_interactive::{self, parse, Parsed, Spec};
use crate::migrate::MigrateError;
use crate::sql::{Auto, Pool};

use super::user::AdminUser;

/// Entry point, called from `migrate::manage::run` for the
/// `"create-admin"` verb. `args` holds the tokens after the verb.
///
/// # Errors
/// Bad arguments, a duplicate username, or a failed hash or insert.
pub async fn create_admin_cmd<W: Write + Send>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    // ---- argument parsing ----------------------------------------
    let mut iter = args.iter();
    let mut username_arg: Option<String> = None;
    let mut password: Option<String> = None;
    let mut generate = false;
    let mut is_superuser = false;
    let mut password_stdin = false;
    let mut skip_validation = false;

    while let Some(token) = iter.next() {
        match token.as_str() {
            "--password" => {
                let v = iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--password requires a value".into())
                })?;
                password = Some(v);
            }
            "--generate" => generate = true,
            "--superuser" => is_superuser = true,
            "--password-stdin" => password_stdin = true,
            "--skip-validation" => skip_validation = true,
            "--help" | "-h" => {
                writeln!(
                    w,
                    "create-admin <username> [--password <p> | --password-stdin | --generate] \
                     [--skip-validation] [--superuser]"
                )?;
                return Ok(());
            }
            arg if arg.starts_with("--") => {
                return Err(MigrateError::Validation(format!(
                    "create-admin: unknown flag `{arg}`"
                )));
            }
            positional if username_arg.is_none() => {
                username_arg = Some(positional.to_owned());
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "create-admin: unexpected positional argument `{other}`"
                )));
            }
        }
    }
    if generate && (password.is_some() || password_stdin) {
        return Err(MigrateError::Validation(
            "create-admin: --generate and --password are mutually exclusive".into(),
        ));
    }

    // ---- resolve username + password from prompts when missing ----
    let username = match username_arg {
        Some(u) => u,
        None => manage_interactive::ask("Username: ")
            .map_err(|e| MigrateError::Validation(format!("prompt failed: {e}")))?
            .ok_or_else(|| {
                MigrateError::Validation(
                    "create-admin requires a username positional argument".into(),
                )
            })?,
    };
    let (plain, generated) = if generate {
        // A 20-char password from the local generator. The bare admin
        // compiles without `tenancy`, so it cannot use that one.
        let plain = generate_password(20);
        (plain, true)
    } else {
        // Same reader and validators as the tenancy verbs (#2589).
        use manage_interactive::{NewPasswordError, PasswordInput};
        let err = |e: NewPasswordError| match e {
            NewPasswordError::Io(e) => MigrateError::Io(e),
            e => MigrateError::Validation(format!("create-admin: {e}")),
        };
        let input = PasswordInput::from_flags(password, password_stdin).map_err(err)?;
        let p = manage_interactive::read_new_password(input, skip_validation, "Password: ")
            .map_err(err)?;
        (p, false)
    };

    // ---- ensure the table exists ---------------------------------
    // A first run on a fresh project should not need a manual
    // `bootstrap` call, so make sure `rustango_admin_users` is there.
    // The shared ensure path: no ERROR in the PG log when the table is
    // there, and a real failure is reported instead of swallowed (#1642).
    let snapshot = crate::migrate::SchemaSnapshot::from_models_forced(&[AdminUser::SCHEMA]);
    crate::migrate::apply_idempotent(pool, &snapshot).await?;

    // ---- reject duplicate username -------------------------------
    use crate::core::{SelectQuery, SqlValue};
    let select = SelectQuery::by_pk(
        AdminUser::SCHEMA,
        "username",
        SqlValue::String(username.clone()),
    );
    let fields: Vec<&'static crate::core::FieldSchema> = AdminUser::SCHEMA.fields.iter().collect();
    let existing = crate::sql::select_one_row_as_json(pool, &select, &fields)
        .await
        .ok()
        .flatten();
    if existing.is_some() {
        return Err(MigrateError::Validation(format!(
            "create-admin: username `{username}` already exists"
        )));
    }

    // ---- hash + insert -------------------------------------------
    let hash = crate::passwords::hash_async(&plain)
        .await
        .map_err(|e| MigrateError::Validation(format!("password hash failed: {e}")))?;
    let mut user = AdminUser {
        id: Auto::Unset,
        username: username.clone(),
        password_hash: hash,
        #[cfg(feature = "admin-sso")]
        email: None,
        is_superuser,
        active: true,
        created_at: chrono::Utc::now(),
        sessions_revoked_at: None,
    };
    user.save_pool(pool)
        .await
        .map_err(|e| MigrateError::Validation(format!("insert failed: {e}")))?;
    let user_id = user.id.get().copied().unwrap_or_default();

    writeln!(
        w,
        "created admin user `{username}` (id={user_id}{super_tag})",
        super_tag = if is_superuser { ", superuser" } else { "" }
    )?;
    if generated {
        writeln!(w, "generated password: {plain}")?;
        writeln!(w, "(save this — it isn't stored or recoverable)")?;
    }
    Ok(())
}

/// Small password generator: `n` chars of URL-safe base64. Local, so
/// the bare admin compiles without `tenancy`.
fn generate_password(n: usize) -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    let s = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf);
    s.chars().take(n).collect()
}

// ---------- account verbs for an existing admin user ----------

const NEW_PASSWORD_SWITCHES: [&str; 3] = ["--password-stdin", "--generate", "--skip-validation"];

/// `reset-password <username>`: a new password without the old one.
///
/// # Errors
/// Bad arguments, an unknown user, a failed hash or write.
pub async fn reset_password_cmd<W: Write + Send>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    const USAGE: &str = "reset-password <username> [--password <p> | --password-stdin | --generate] [--skip-validation]";
    let parsed = parse(
        args,
        &Spec {
            verb: "reset-password",
            usage: USAGE,
            switches: &NEW_PASSWORD_SWITCHES,
            valued: &["--password"],
            max_positionals: 1,
        },
    )?;
    let username = parsed.required(0, USAGE)?;
    let (plain, generated) = new_password("reset-password", &parsed)?;
    let user = admin_named(pool, "reset-password", &username).await?;
    store_password(pool, "reset-password", &user, &plain).await?;
    audit_account(pool, &user, "password_reset").await?;
    writeln!(w, "password reset for admin user `{username}`")?;
    if generated {
        manage_interactive::print_secret_once(w, "generated password", &plain)?;
    }
    Ok(())
}

/// `change-password <username>`: a new password, after the current one.
///
/// # Errors
/// Bad arguments, an unknown user, a wrong current password, a failed write.
pub async fn change_password_cmd<W: Write + Send>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    const USAGE: &str = "change-password <username> [--current <p> | --current-stdin] [--password <p> | --password-stdin | --generate] [--skip-validation]";
    let parsed = parse(
        args,
        &Spec {
            verb: "change-password",
            usage: USAGE,
            switches: &[
                "--current-stdin",
                "--password-stdin",
                "--generate",
                "--skip-validation",
            ],
            valued: &["--current", "--password"],
            max_positionals: 1,
        },
    )?;
    let username = parsed.required(0, USAGE)?;
    let current = manage_interactive::read_current_password(
        parsed.value("--current")?.map(str::to_owned),
        parsed.has("--current-stdin"),
    )
    .map_err(|e| password_error("change-password", e))?;
    let (plain, generated) = new_password("change-password", &parsed)?;
    if plain == current {
        return Err(MigrateError::Validation(
            "change-password: the new password must differ from the current one".into(),
        ));
    }
    let user = admin_named(pool, "change-password", &username).await?;
    let ok = crate::passwords::verify_async(&current, &user.password_hash)
        .await
        .map_err(|e| MigrateError::Validation(format!("change-password: {e}")))?;
    if !ok {
        return Err(MigrateError::Validation(
            "change-password: current password did not match".into(),
        ));
    }
    store_password(pool, "change-password", &user, &plain).await?;
    audit_account(pool, &user, "password_change").await?;
    writeln!(w, "password changed for admin user `{username}`")?;
    if generated {
        manage_interactive::print_secret_once(w, "generated password", &plain)?;
    }
    Ok(())
}

/// `set-admin-active <username> --on|--off`. Off also ends every session.
///
/// # Errors
/// Bad arguments, an unknown user, a failed write.
pub async fn set_admin_active_cmd<W: Write + Send>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    use crate::core::Column as _;
    use crate::sql::UpdaterPool as _;
    const USAGE: &str = "set-admin-active <username> --on|--off";
    let parsed = parse(
        args,
        &Spec {
            verb: "set-admin-active",
            usage: USAGE,
            switches: &["--on", "--off"],
            valued: &[],
            max_positionals: 1,
        },
    )?;
    let username = parsed.required(0, USAGE)?;
    let active = parsed
        .on_off()?
        .ok_or_else(|| MigrateError::Validation(format!("usage: {USAGE}")))?;
    let user = admin_named(pool, "set-admin-active", &username).await?;
    if user.active == active {
        let state = if active { "active" } else { "inactive" };
        writeln!(w, "`{username}` was already {state}")?;
        return Ok(());
    }
    let mut update = AdminUser::objects()
        .where_(AdminUser::id.eq(admin_id(&user)?))
        .update()
        .set_typed(AdminUser::active.set(active));
    if !active {
        // Reactivating later must not revive a cookie minted before this.
        update = update.set_typed(AdminUser::sessions_revoked_at.set(Some(chrono::Utc::now())));
    }
    update.execute_pool(pool).await?;
    let action = if active { "activate" } else { "deactivate" };
    audit_account(pool, &user, action).await?;
    writeln!(w, "{action}d `{username}`")?;
    Ok(())
}

/// `reset-admin-totp <username>`: drop the user's authenticator, for a lost device.
///
/// # Errors
/// Bad arguments, an unknown user, a failed write.
#[cfg(feature = "totp")]
pub async fn reset_admin_totp_cmd<W: Write + Send>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    use super::totp_store::{reset_in, AdminTotp};
    const USAGE: &str = "reset-admin-totp <username>";
    let parsed = parse(
        args,
        &Spec {
            verb: "reset-admin-totp",
            usage: USAGE,
            switches: &[],
            valued: &[],
            max_positionals: 1,
        },
    )?;
    let username = parsed.required(0, USAGE)?;
    let user = admin_named(pool, "reset-admin-totp", &username).await?;
    super::totp_store::ensure_table(pool).await?;
    let id = admin_id(&user)?;
    // A session opened with the lost device must not outlive it. Ended
    // first, so a retry after a failed write still ends them.
    crate::session::revoke_sessions::<AdminUser>(pool, id, user.sessions_revoked_at, 0).await?;
    if reset_in::<AdminTotp>(pool, id).await? == 0 {
        writeln!(
            w,
            "`{username}` has no two-factor device; their sessions are ended"
        )?;
        return Ok(());
    }
    audit_account(pool, &user, "totp_reset").await?;
    writeln!(
        w,
        "two-factor device removed for `{username}`; their sessions are ended and they sign in \
         with the password alone"
    )?;
    Ok(())
}

fn password_error(verb: &str, e: manage_interactive::NewPasswordError) -> MigrateError {
    match e {
        manage_interactive::NewPasswordError::Io(e) => MigrateError::Io(e),
        e => MigrateError::Validation(format!("{verb}: {e}")),
    }
}

/// A generated password, or one read and validated like the forms.
fn new_password(verb: &str, parsed: &Parsed) -> Result<(String, bool), MigrateError> {
    use manage_interactive::PasswordInput;
    let password = parsed.value("--password")?.map(str::to_owned);
    let stdin = parsed.has("--password-stdin");
    if parsed.has("--generate") {
        if password.is_some() || stdin {
            return Err(MigrateError::Validation(format!(
                "{verb}: --generate and --password are mutually exclusive"
            )));
        }
        return Ok((generate_password(20), true));
    }
    let input = PasswordInput::from_flags(password, stdin).map_err(|e| password_error(verb, e))?;
    manage_interactive::read_new_password(input, parsed.has("--skip-validation"), "New password: ")
        .map(|p| (p, false))
        .map_err(|e| password_error(verb, e))
}

async fn admin_named(pool: &Pool, verb: &str, username: &str) -> Result<AdminUser, MigrateError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    AdminUser::objects()
        .where_(AdminUser::username.eq(username.to_owned()))
        .fetch(pool)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| MigrateError::Validation(format!("{verb}: no admin user `{username}`")))
}

fn admin_id(user: &AdminUser) -> Result<i64, MigrateError> {
    user.id
        .get()
        .copied()
        .ok_or_else(|| MigrateError::Validation("admin user row has no id".into()))
}

/// Over the hash just read, so a change that landed meanwhile is not overwritten.
async fn store_password(
    pool: &Pool,
    verb: &str,
    user: &AdminUser,
    plain: &str,
) -> Result<(), MigrateError> {
    let hash = crate::passwords::hash_async(plain)
        .await
        .map_err(|e| MigrateError::Validation(format!("{verb}: password hash failed: {e}")))?;
    let q = crate::passwords::rehash_update(
        AdminUser::SCHEMA,
        admin_id(user)?,
        &user.password_hash,
        &hash,
        None,
    );
    if crate::sql::update_pool(pool, &q).await? != 1 {
        return Err(MigrateError::Validation(format!(
            "{verb}: `{}`'s password changed meanwhile; try again",
            user.username
        )));
    }
    Ok(())
}

/// One `source = cli` audit row on the admin user's own entry.
async fn audit_account(pool: &Pool, user: &AdminUser, action: &str) -> Result<(), MigrateError> {
    use crate::audit::{AuditOp, AuditSource, PendingEntry};
    crate::audit::ensure_table_pool(pool).await?;
    let entry = PendingEntry {
        entity_table: AdminUser::SCHEMA.table,
        entity_pk: admin_id(user)?.to_string(),
        operation: AuditOp::Action,
        source: AuditSource::Custom("cli".into()),
        changes: serde_json::json!({ "action": action }),
    };
    crate::audit::emit_one_pool(pool, &entry).await?;
    Ok(())
}
