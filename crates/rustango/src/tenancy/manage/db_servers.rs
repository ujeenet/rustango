//! Saved database server verbs — `add-db-server`, `list-db-servers`,
//! `test-db-server`, `edit-db-server`, `remove-db-server`.
//!
//! The password never comes from argv: a hidden prompt, or the variable
//! named by `--password-env`. No output shows it.

use std::io::{IsTerminal as _, Write};
use std::time::Duration;

use sqlx::Database;

use crate::sql::LoginPassword;
use crate::tenancy::db_server::{self, DbServerSpec, PasswordChange, TlsMode};
use crate::tenancy::error::TenancyError;
use crate::tenancy::pools::TenantPools;
use crate::tenancy::BackendKind;

use super::args::{parse, Parsed, Spec};

/// How long `test-db-server` waits for the server.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

const ADD: Spec<'static> = Spec {
    verb: "add-db-server",
    usage: "add-db-server <name> [--backend <b>] [--host <h>] [--port <n>] [--user <u>] \
            [--tls <mode>, default require] [--maintenance-db <db>] [--sqlite-dir <dir>] \
            [--password-env <VAR> | --no-password] [--inactive]",
    switches: &["--no-password", "--inactive"],
    valued: &[
        "--backend",
        "--host",
        "--port",
        "--user",
        "--tls",
        "--maintenance-db",
        "--sqlite-dir",
        "--password-env",
    ],
    max_positionals: 1,
};

const EDIT: Spec<'static> = Spec {
    verb: "edit-db-server",
    usage: "edit-db-server <name> [--host <h>] [--port <n>] [--user <u>] [--tls <mode>|none] \
            [--maintenance-db <db>|none] [--sqlite-dir <dir>] \
            [--password-env <VAR> | --prompt-password | --clear-password] [--on|--off]",
    switches: &["--prompt-password", "--clear-password", "--on", "--off"],
    valued: &[
        "--host",
        "--port",
        "--user",
        "--tls",
        "--maintenance-db",
        "--sqlite-dir",
        "--password-env",
    ],
    max_positionals: 1,
};

fn name_spec(verb: &'static str) -> Spec<'static> {
    Spec {
        verb,
        usage: "<verb> <name>",
        switches: &[],
        valued: &[],
        max_positionals: 1,
    }
}

fn port(raw: &str) -> Result<u16, TenancyError> {
    raw.trim().parse().map_err(|_| {
        TenancyError::Validation(format!("port `{raw}` is not a number from 1 to 65535"))
    })
}

/// `none` clears an optional value.
fn clearable(raw: &str) -> Option<String> {
    (!raw.trim().eq_ignore_ascii_case("none")).then(|| raw.to_owned())
}

/// The password named by `--password-env`; the value is never echoed.
fn password_from_env(var: &str) -> Result<LoginPassword, TenancyError> {
    let value = std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| TenancyError::Validation(format!("${var} is not set or empty")))?;
    LoginPassword::new(value)
        .map_err(|_| TenancyError::Validation(format!("${var} is not a usable password")))
}

/// Asked twice with echo off; `None` when the answer is blank.
fn password_from_prompt() -> Result<Option<LoginPassword>, TenancyError> {
    if !std::io::stdin().is_terminal() {
        return Err(TenancyError::Validation(
            "no terminal to ask for the password: pass --password-env <VAR> or --no-password"
                .into(),
        ));
    }
    let ask = crate::manage_interactive::ask_password;
    let Some(pw) = ask("Admin password (blank for none): ").map_err(TenancyError::Io)? else {
        return Ok(None);
    };
    if ask("Admin password (again): ")
        .map_err(TenancyError::Io)?
        .as_deref()
        != Some(pw.as_str())
    {
        return Err(TenancyError::Validation("passwords didn't match".into()));
    }
    LoginPassword::new(pw)
        .map(Some)
        .map_err(|_| TenancyError::Validation("that password is not usable".into()))
}

/// The server name, or the answer to a prompt on a terminal (the menu
/// runs these verbs without positionals).
fn name_arg(p: &Parsed, usage: &str) -> Result<String, TenancyError> {
    match p.positional(0) {
        Some(n) => Ok(n.clone()),
        None => crate::manage_interactive::ask("Server name: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| TenancyError::Validation(format!("usage: {usage}"))),
    }
}

/// A blank prompt answer keeps the saved password; only
/// `--clear-password` removes it.
fn prompted_change(answer: Option<LoginPassword>) -> PasswordChange {
    answer.map_or(PasswordChange::Keep, PasswordChange::Set)
}

fn apply_common(spec: &mut DbServerSpec, p: &Parsed) -> Result<(), TenancyError> {
    if let Some(v) = p.value("--host")? {
        spec.host = Some(v.to_owned());
    }
    if let Some(v) = p.value("--port")? {
        spec.port = clearable(v).map(|v| port(&v)).transpose()?;
    }
    if let Some(v) = p.value("--user")? {
        spec.admin_user = Some(v.to_owned());
    }
    if let Some(v) = p.value("--tls")? {
        spec.tls = clearable(v).map(|v| TlsMode::parse(&v)).transpose()?;
    }
    if let Some(v) = p.value("--maintenance-db")? {
        spec.maintenance_db = clearable(v);
    }
    if let Some(v) = p.value("--sqlite-dir")? {
        spec.sqlite_dir = Some(v.to_owned());
    }
    Ok(())
}

pub(super) async fn add_db_server<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let p = parse(args, &ADD)?;
    let registry = pools.registry_pool();
    let backend = match p.value("--backend")? {
        Some(b) => BackendKind::parse(b)
            .map_err(|b| TenancyError::Validation(format!("unknown backend `{b}`")))?,
        None => BackendKind::parse(registry.dialect().name()).unwrap_or_default(),
    };
    let mut spec = DbServerSpec {
        name: name_arg(&p, ADD.usage)?,
        backend,
        active: !p.has("--inactive"),
        ..DbServerSpec::default()
    };
    apply_common(&mut spec, &p)?;
    let password = match (p.value("--password-env")?, p.has("--no-password")) {
        (Some(_), true) => {
            return Err(TenancyError::Validation(
                "--password-env and --no-password contradict each other — pass one".into(),
            ))
        }
        (Some(var), false) => Some(password_from_env(var)?),
        (None, true) => None,
        (None, false) if backend == BackendKind::Sqlite => None,
        (None, false) => password_from_prompt()?,
    };
    let row = db_server::add(&registry, &spec, password.as_ref()).await?;
    writeln!(w, "saved database server `{}`", row.name)?;
    writeln!(w, "  check it with: test-db-server {}", row.name)?;
    Ok(())
}

pub(super) async fn edit_db_server<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let p = parse(args, &EDIT)?;
    let name = name_arg(&p, EDIT.usage)?;
    let registry = pools.registry_pool();
    let row = db_server::get(&registry, &name).await?;
    let mut spec = row.spec()?;
    apply_common(&mut spec, &p)?;
    if let Some(active) = p.on_off()? {
        spec.active = active;
    }
    let chosen = [
        p.value("--password-env")?.is_some(),
        p.has("--prompt-password"),
        p.has("--clear-password"),
    ];
    if chosen.iter().filter(|c| **c).count() > 1 {
        return Err(TenancyError::Validation(
            "pass one of --password-env, --prompt-password and --clear-password".into(),
        ));
    }
    let password = if let Some(var) = p.value("--password-env")? {
        PasswordChange::Set(password_from_env(var)?)
    } else if p.has("--prompt-password") {
        prompted_change(password_from_prompt()?)
    } else if p.has("--clear-password") {
        PasswordChange::Clear
    } else {
        PasswordChange::Keep
    };
    let seen = row.updated_at.get().copied();
    let (_, changed) = db_server::update(&registry, &name, &spec, password, seen).await?;
    if changed.is_empty() {
        writeln!(w, "`{name}`: nothing changed")?;
    } else {
        writeln!(w, "updated `{name}`: {}", changed.join(", "))?;
    }
    Ok(())
}

pub(super) async fn list_db_servers<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    super::args::no_args(args, "list-db-servers")?;
    let rows = db_server::list(&pools.registry_pool()).await?;
    if rows.is_empty() {
        writeln!(w, "(no database servers — add one with `add-db-server`)")?;
        return Ok(());
    }
    writeln!(
        w,
        "{:<24} {:<9} {:<36} {:<16} {:<12} {:<9} active",
        "name", "backend", "where", "user", "tls", "password"
    )?;
    writeln!(w, "{}", "-".repeat(118))?;
    for s in rows {
        let at = match (&s.host, &s.sqlite_dir) {
            (Some(h), _) => s.port.map_or_else(|| h.clone(), |p| format!("{h}:{p}")),
            (None, Some(d)) => d.clone(),
            (None, None) => "-".into(),
        };
        writeln!(
            w,
            "{:<24} {:<9} {:<36} {:<16} {:<12} {:<9} {}",
            s.name,
            s.backend,
            at,
            s.admin_user.as_deref().unwrap_or("-"),
            s.tls_mode.as_deref().unwrap_or("-"),
            if s.has_password() { "saved" } else { "none" },
            s.active
        )?;
    }
    Ok(())
}

pub(super) async fn test_db_server<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let p = parse(args, &name_spec("test-db-server"))?;
    let name = name_arg(&p, "test-db-server <name>")?;
    let row = db_server::get(&pools.registry_pool(), &name).await?;
    let report = row.probe(PROBE_TIMEOUT).await?;
    let yes = |b: bool| if b { "yes" } else { "no" };
    writeln!(w, "{name} ({})", report.backend)?;
    if let Some(warning) = row.tls_warning() {
        writeln!(w, "  warning:          {warning}")?;
    }
    writeln!(w, "  login:            {}", yes(report.can_login))?;
    if let Some(e) = &report.login_error {
        writeln!(w, "  login error:      {e}")?;
    }
    if let Some(v) = &report.server_version {
        writeln!(w, "  server version:   {v}")?;
    }
    let rights = report.rights();
    for c in rights.checks() {
        writeln!(w, "  {}: {}", c.label, c.has.as_str())?;
    }
    if let Some(note) = rights.unknown_note() {
        writeln!(w, "  note: {note}")?;
    }
    if rights.is_ready() {
        writeln!(w, "ready: tenant databases can be created here")?;
    } else {
        for line in rights.refusal_lines("not ready") {
            writeln!(w, "{line}")?;
        }
        let subject = format!("database server `{name}` cannot create tenant databases");
        return Err(TenancyError::Validation(rights.refusal(&subject)));
    }
    Ok(())
}

pub(super) async fn remove_db_server<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let p = parse(args, &name_spec("remove-db-server"))?;
    let name = name_arg(&p, "remove-db-server <name>")?;
    db_server::remove(&pools.registry_pool(), &name, pools.secrets()).await?;
    writeln!(w, "removed database server `{name}`")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_prompt_keeps_the_password() {
        assert!(matches!(prompted_change(None), PasswordChange::Keep));
        let pw = LoginPassword::new("x").unwrap();
        assert!(matches!(prompted_change(Some(pw)), PasswordChange::Set(_)));
    }
}
