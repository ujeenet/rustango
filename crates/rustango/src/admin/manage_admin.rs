//! The `create-admin` CLI verb: create an `AdminUser` row for a
//! project that uses `admin::Builder::with_session_auth`.
//!
//! It takes a plain `&Pool`, with no tenancy resolver, writes to
//! `rustango_admin_users` (creating the table if needed), and hashes
//! the password with `crate::passwords::hash`.
//!
//! Usage:
//!
//! ```text
//! cargo run -- create-admin alice                          # prompts for password
//! cargo run -- create-admin alice --password secret        # explicit
//! cargo run -- create-admin alice --generate               # mints a random one + prints
//! cargo run -- create-admin alice --password secret --superuser
//! ```

use std::io::Write;

use crate::core::Model;
use crate::manage_interactive;
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
            "--help" | "-h" => {
                writeln!(
                    w,
                    "create-admin <username> [--password <p> | --generate] [--superuser]"
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
    if generate && password.is_some() {
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
        let p = match password {
            Some(p) => p,
            None => manage_interactive::ask_password("Password: ")
                .map_err(|e| MigrateError::Validation(format!("prompt failed: {e}")))?
                .ok_or_else(|| {
                    MigrateError::Validation(
                        "create-admin requires --password (or run interactively)".into(),
                    )
                })?,
        };
        (p, false)
    };

    // ---- ensure the table exists ---------------------------------
    // A first run on a fresh project should not need a manual
    // `bootstrap` call, so make sure `rustango_admin_users` is there.
    use crate::migrate::ddl;
    let dialect = pool.dialect();
    let sql = ddl::create_table_sql_with_dialect(dialect, AdminUser::SCHEMA);
    // Support for "IF NOT EXISTS" varies by driver, so just run it and
    // ignore an "already exists" failure, which is harmless here.
    let _ = crate::sql::raw_execute_pool(pool, &sql, vec![]).await;

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
    let hash = crate::passwords::hash(&plain)
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
