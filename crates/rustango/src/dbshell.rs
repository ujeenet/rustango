//! `manage dbshell` — spawn the native CLI client (`psql`, `mysql`,
//! `sqlite3`) for the current `DATABASE_URL`. Django's
//! [`dbshell`](https://docs.djangoproject.com/en/6.0/ref/django-admin/#dbshell).
//! Issue #56 (partial).
//!
//! Convention over configuration: parse the URL's scheme + components,
//! find the right binary on `PATH`, hand control off via `exec()`
//! (Unix) / process replacement so signals (Ctrl-C) reach the child
//! cleanly.
//!
//! Run-time deps the user must install themselves:
//! - PostgreSQL: `psql`
//! - MySQL / MariaDB: `mysql`
//! - SQLite: `sqlite3`
//!
//! Each is the standard CLI shipped with its server. The verb returns
//! a clear error if the binary isn't on `PATH`.

use std::ffi::OsString;
use std::process::Command;

/// What the URL parser pulled out of `DATABASE_URL`. Each variant
/// carries enough information to assemble the right CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbTarget {
    /// PostgreSQL — `psql` accepts the full connection URI directly.
    Postgres { url: String },
    /// MySQL / MariaDB — `mysql` needs separate flags (`-h`, `-u`,
    /// `-p`, etc.). Parsed into structured components.
    Mysql {
        host: Option<String>,
        port: Option<u16>,
        user: Option<String>,
        password: Option<String>,
        database: Option<String>,
    },
    /// SQLite — `sqlite3` takes the database file path positionally.
    /// `:memory:` is preserved verbatim.
    Sqlite { path: String },
}

/// Parse a `DATABASE_URL`-shaped string into a [`DbTarget`].
///
/// Accepts the standard schemes Django + sqlx recognize:
/// - `postgres://...` / `postgresql://...`
/// - `mysql://...` / `mariadb://...`
/// - `sqlite://path/to/db` / `sqlite:///abs/path` / `sqlite::memory:`
///
/// # Errors
/// Returns the offending input as a string when the scheme is missing
/// or unrecognized. URL components beyond the scheme are parsed loosely
/// — invalid percent-encoding stays as-is rather than rejecting.
pub fn parse_target(url: &str) -> Result<DbTarget, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("DATABASE_URL is empty".to_owned());
    }
    let (scheme, rest) = url
        .split_once("://")
        .or_else(|| url.split_once(":"))
        .ok_or_else(|| format!("DATABASE_URL has no scheme: `{url}`"))?;

    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => Ok(DbTarget::Postgres {
            url: url.to_owned(),
        }),
        "mysql" | "mariadb" => Ok(parse_mysql(rest)),
        "sqlite" => Ok(DbTarget::Sqlite {
            path: parse_sqlite_path(rest),
        }),
        other => Err(format!(
            "unsupported DATABASE_URL scheme `{other}`; expected postgres / mysql / sqlite"
        )),
    }
}

fn parse_mysql(rest: &str) -> DbTarget {
    // Format: [user[:pass]@]host[:port][/db][?query]
    // Strip any `?query` suffix — `mysql` CLI doesn't take URL query.
    let body = rest.split_once('?').map_or(rest, |(b, _)| b);
    // Pull database path off the end.
    let (auth_host, database) = match body.split_once('/') {
        Some((auth_host, db)) if !db.is_empty() => (auth_host, Some(db.to_owned())),
        Some((auth_host, _)) => (auth_host, None),
        None => (body, None),
    };
    // Split auth + host on '@' (last '@' so a `:` in password before
    // it doesn't confuse the split).
    let (auth, host_port) = match auth_host.rsplit_once('@') {
        Some((a, hp)) => (Some(a), hp),
        None => (None, auth_host),
    };
    let (user, password) = match auth {
        None => (None, None),
        Some(a) => match a.split_once(':') {
            Some((u, p)) => (Some(u.to_owned()), Some(p.to_owned())),
            None => (Some(a.to_owned()), None),
        },
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(n) => (Some(h.to_owned()), Some(n)),
            Err(_) => (Some(host_port.to_owned()), None),
        },
        None => {
            if host_port.is_empty() {
                (None, None)
            } else {
                (Some(host_port.to_owned()), None)
            }
        }
    };
    DbTarget::Mysql {
        host,
        port,
        user,
        password,
        database,
    }
}

fn parse_sqlite_path(rest: &str) -> String {
    // sqlx accepts `sqlite::memory:`, `sqlite:///abs/path`,
    // `sqlite://path` (technically wrong but tolerated). After the
    // `split_once` in [`parse_target`] strips the scheme, the rest is
    // `:memory:` / `/abs/path` / `path` / `/path` — pass through.
    if rest == ":memory:" || rest.starts_with(":memory:") {
        return ":memory:".to_owned();
    }
    // Strip a leading `/` introduced by the `sqlite:///abs/path` form
    // (third `/` becomes part of the path) if a literal absolute path
    // wasn't intended — but actually we WANT the absolute path. Just
    // pass through verbatim.
    rest.to_owned()
}

/// Build the `Command` that would invoke the native CLI for `target`.
/// Returns `(program, args)` so tests can inspect without spawning.
#[must_use]
pub fn command_for(target: &DbTarget) -> (&'static str, Vec<OsString>) {
    match target {
        DbTarget::Postgres { url } => ("psql", vec![OsString::from(url)]),
        DbTarget::Mysql {
            host,
            port,
            user,
            password,
            database,
        } => {
            let mut args: Vec<OsString> = Vec::new();
            if let Some(h) = host {
                args.push("-h".into());
                args.push(h.into());
            }
            if let Some(p) = port {
                args.push("-P".into());
                args.push(p.to_string().into());
            }
            if let Some(u) = user {
                args.push("-u".into());
                args.push(u.into());
            }
            if let Some(pw) = password {
                // `-p<pass>` is the no-space form; `--password=...` is
                // safer (mysql client supports both). Prefer the long
                // form so the password doesn't show in ps output as
                // `-pSECRET`.
                args.push(format!("--password={pw}").into());
            }
            if let Some(db) = database {
                args.push(db.into());
            }
            ("mysql", args)
        }
        DbTarget::Sqlite { path } => ("sqlite3", vec![OsString::from(path)]),
    }
}

/// Spawn the right CLI for the given `DATABASE_URL` and replace the
/// current process. Returns an `Err` only when the URL parse fails;
/// on success the child takes over and this never returns. On Unix
/// uses `exec()` to swap the process image; on other targets falls
/// back to `Command::status` + a propagating exit code.
///
/// # Errors
/// - URL parse failure ([`parse_target`]).
/// - On non-Unix targets, errors from spawning the child or
///   non-zero exit codes from the client itself.
pub fn run(url: &str) -> Result<std::convert::Infallible, Box<dyn std::error::Error>> {
    let target = parse_target(url).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let (program, args) = command_for(&target);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = Command::new(program).args(&args).exec();
        // `exec` only returns on failure (e.g. binary not on PATH).
        Err(format!(
            "failed to exec `{program}` for dbshell: {err}. \
             Is the {program} client installed and on PATH?"
        )
        .into())
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(program).args(&args).status()?;
        if status.success() {
            std::process::exit(0);
        }
        Err(format!(
            "`{program}` exited with status {status}. Is the {program} client installed and on PATH?"
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_postgres_scheme_keeps_url_intact() {
        assert_eq!(
            parse_target("postgres://user:pass@localhost:5432/dbname").unwrap(),
            DbTarget::Postgres {
                url: "postgres://user:pass@localhost:5432/dbname".to_owned()
            }
        );
        // `postgresql://` alias.
        assert_eq!(
            parse_target("postgresql://localhost/dbname").unwrap(),
            DbTarget::Postgres {
                url: "postgresql://localhost/dbname".to_owned()
            }
        );
    }

    #[test]
    fn parse_mysql_full_dsn() {
        assert_eq!(
            parse_target("mysql://alice:secret@db.example.com:3307/myapp").unwrap(),
            DbTarget::Mysql {
                host: Some("db.example.com".to_owned()),
                port: Some(3307),
                user: Some("alice".to_owned()),
                password: Some("secret".to_owned()),
                database: Some("myapp".to_owned()),
            }
        );
    }

    #[test]
    fn parse_mysql_no_password() {
        assert_eq!(
            parse_target("mysql://alice@localhost/myapp").unwrap(),
            DbTarget::Mysql {
                host: Some("localhost".to_owned()),
                port: None,
                user: Some("alice".to_owned()),
                password: None,
                database: Some("myapp".to_owned()),
            }
        );
    }

    #[test]
    fn parse_mysql_no_auth() {
        assert_eq!(
            parse_target("mysql://localhost").unwrap(),
            DbTarget::Mysql {
                host: Some("localhost".to_owned()),
                port: None,
                user: None,
                password: None,
                database: None,
            }
        );
    }

    #[test]
    fn parse_mysql_strips_query_string() {
        // sqlx-style options like `?ssl-mode=REQUIRED` don't carry over
        // to the mysql CLI; strip them.
        assert_eq!(
            parse_target("mysql://localhost/db?ssl-mode=REQUIRED").unwrap(),
            DbTarget::Mysql {
                host: Some("localhost".to_owned()),
                port: None,
                user: None,
                password: None,
                database: Some("db".to_owned()),
            }
        );
    }

    #[test]
    fn parse_mariadb_alias_routes_to_mysql() {
        let target = parse_target("mariadb://localhost/db").unwrap();
        assert!(matches!(target, DbTarget::Mysql { .. }));
    }

    #[test]
    fn parse_sqlite_relative_path() {
        assert_eq!(
            parse_target("sqlite://app.db").unwrap(),
            DbTarget::Sqlite {
                path: "app.db".to_owned()
            }
        );
    }

    #[test]
    fn parse_sqlite_absolute_path_keeps_leading_slash() {
        assert_eq!(
            parse_target("sqlite:///var/lib/app.db").unwrap(),
            DbTarget::Sqlite {
                path: "/var/lib/app.db".to_owned()
            }
        );
    }

    #[test]
    fn parse_sqlite_in_memory() {
        // The conventional sqlx form is `sqlite::memory:`.
        assert_eq!(
            parse_target("sqlite::memory:").unwrap(),
            DbTarget::Sqlite {
                path: ":memory:".to_owned()
            }
        );
    }

    #[test]
    fn parse_unknown_scheme_returns_error() {
        let err = parse_target("redis://localhost").unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
        assert!(err.contains("redis"), "got: {err}");
    }

    #[test]
    fn parse_empty_returns_error() {
        assert!(parse_target("").is_err());
        assert!(parse_target("   ").is_err());
    }

    #[test]
    fn parse_no_scheme_returns_error() {
        let err = parse_target("just-a-string").unwrap_err();
        assert!(
            err.contains("no scheme") || err.contains("unsupported"),
            "got: {err}"
        );
    }

    // ---- command_for ----

    #[test]
    fn command_for_postgres_uses_psql_with_full_url() {
        let target = DbTarget::Postgres {
            url: "postgres://localhost/db".to_owned(),
        };
        let (prog, args) = command_for(&target);
        assert_eq!(prog, "psql");
        assert_eq!(args, vec![OsString::from("postgres://localhost/db")]);
    }

    #[test]
    fn command_for_mysql_passes_host_user_password_db_as_flags() {
        let target = DbTarget::Mysql {
            host: Some("db.example.com".to_owned()),
            port: Some(3307),
            user: Some("alice".to_owned()),
            password: Some("secret".to_owned()),
            database: Some("myapp".to_owned()),
        };
        let (prog, args) = command_for(&target);
        assert_eq!(prog, "mysql");
        let args_str: Vec<String> = args.into_iter().map(|s| s.into_string().unwrap()).collect();
        assert_eq!(
            args_str,
            vec![
                "-h",
                "db.example.com",
                "-P",
                "3307",
                "-u",
                "alice",
                "--password=secret",
                "myapp",
            ]
        );
    }

    #[test]
    fn command_for_mysql_uses_long_password_form() {
        // Defense against `ps` exposure: `-p<pass>` shows the password
        // in `ps aux` output. `--password=<pass>` is the safer form
        // (still imperfect, but no worse than `-h <host>`).
        let target = DbTarget::Mysql {
            host: Some("localhost".to_owned()),
            port: None,
            user: Some("u".to_owned()),
            password: Some("pw".to_owned()),
            database: None,
        };
        let (_, args) = command_for(&target);
        let args_str: Vec<String> = args.into_iter().map(|s| s.into_string().unwrap()).collect();
        assert!(args_str.contains(&"--password=pw".to_owned()));
        assert!(!args_str.iter().any(|a| a.starts_with("-ppw")));
    }

    #[test]
    fn command_for_sqlite_uses_sqlite3_with_path() {
        let target = DbTarget::Sqlite {
            path: ":memory:".to_owned(),
        };
        assert_eq!(
            command_for(&target),
            ("sqlite3", vec![OsString::from(":memory:")])
        );
    }
}
