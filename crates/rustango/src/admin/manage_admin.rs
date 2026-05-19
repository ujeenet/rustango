//! `manage create-admin` CLI verb — bootstrap an `AdminUser` row for
//! projects using `admin::Builder::with_session_auth` (#253 slice B).
//!
//! Modelled on `tenancy::manage::users::create_user_cmd` but:
//! - takes a plain `&Pool` (no tenancy resolver needed),
//! - writes to `rustango_admin_users` (auto-created if absent),
//! - uses `crate::passwords::hash` for the password.
//!
//! Usage (from any project pairing the bare admin with session
//! auth):
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

/// Dispatch entry-point — wired into `migrate::manage::run` under
/// the `"create-admin"` arm. `args` is the slice AFTER the verb name.
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
        // Reuse the tenancy password generator when the `tenancy`
        // feature is on; the bare admin compiles without `tenancy`,
        // so fall back to a tiny local generator using `rand`. Both
        // paths produce a 20-char URL-safe-base64-trimmed password.
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
    // First run on a fresh sqlite project shouldn't require manually
    // calling `bootstrap` — the migrate verb that just ran already
    // applied any project migrations, so this is the right place to
    // make sure `rustango_admin_users` is present.
    use crate::migrate::ddl;
    let dialect = pool.dialect();
    let sql = ddl::create_table_sql_with_dialect(dialect, AdminUser::SCHEMA);
    // Driver-specific "IF NOT EXISTS" support varies; we just attempt
    // and absorb "already exists" failures since they're benign here.
    let _ = crate::sql::raw_execute_pool(pool, &sql, vec![]).await;

    // ---- reject duplicate username -------------------------------
    use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
    let select = SelectQuery {
        model: AdminUser::SCHEMA,
        where_clause: WhereExpr::Predicate(Filter {
            column: "username",
            op: Op::Eq,
            value: SqlValue::String(username.clone()),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
        lock_mode: None,
        compound: vec![],
        projection: None,
        distinct: None,
    };
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

/// Tiny local password generator — 20 chars from URL-safe base64.
/// Independent of tenancy so the bare admin compiles without it.
fn generate_password(n: usize) -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    let s = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf);
    s.chars().take(n).collect()
}
