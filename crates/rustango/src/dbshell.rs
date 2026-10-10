//! `manage dbshell`: open the native CLI client for the current
//! `DATABASE_URL`.
//!
//! The URL scheme picks the client, and the process is replaced with
//! `exec()` on Unix so Ctrl-C reaches the child.
//!
//! The client itself must already be installed: `psql` for PostgreSQL,
//! `mysql` for MySQL and MariaDB, `sqlite3` for SQLite. If it is not on
//! `PATH`, the verb says so.

use std::ffi::OsString;
use std::process::Command;

/// What the parser found in `DATABASE_URL`, split into the parts each
/// client needs.
///
/// The password is held apart from the other parts on purpose: a
/// password in `argv` is visible to anyone who runs `ps aux`, so it
/// goes to the child through `PGPASSWORD` or `MYSQL_PWD` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbTarget {
    /// PostgreSQL. The password goes out through `PGPASSWORD`.
    Postgres {
        host: Option<String>,
        port: Option<u16>,
        user: Option<String>,
        password: Option<String>,
        database: Option<String>,
    },
    /// MySQL or MariaDB. `mysql` takes separate flags, and the
    /// password goes out through `MYSQL_PWD`.
    Mysql {
        host: Option<String>,
        port: Option<u16>,
        user: Option<String>,
        password: Option<String>,
        database: Option<String>,
    },
    /// SQLite. `sqlite3` takes the file path, and `:memory:` is kept
    /// as is.
    Sqlite { path: String },
}

/// Parse a `DATABASE_URL` into a [`DbTarget`].
///
/// Accepted schemes: `postgres`, `postgresql`, `mysql`, `mariadb` and
/// `sqlite` (`sqlite://path`, `sqlite:///abs/path`, `sqlite::memory:`).
///
/// # Errors
/// Returns a message naming the input when the scheme is missing or
/// unknown. The rest of the URL is parsed loosely, so bad
/// percent-encoding is kept rather than rejected.
pub fn parse_target(url: &str) -> Result<DbTarget, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("DATABASE_URL is empty".to_owned());
    }
    // The scheme ends at the first `:`; a later `://` (say, in the query)
    // would put userinfo in it, and it is echoed below.
    let (scheme, after) = url
        .split_once(':')
        .ok_or_else(|| "DATABASE_URL has no scheme".to_owned())?;
    let rest = after.strip_prefix("//").unwrap_or(after);

    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => Ok(parse_userinfo_host_db(rest, |h, p, u, pw, db| {
            DbTarget::Postgres {
                host: h,
                port: p,
                user: u,
                password: pw,
                database: db,
            }
        })),
        "mysql" | "mariadb" => Ok(parse_userinfo_host_db(rest, |h, p, u, pw, db| {
            DbTarget::Mysql {
                host: h,
                port: p,
                user: u,
                password: pw,
                database: db,
            }
        })),
        "sqlite" => Ok(DbTarget::Sqlite {
            path: parse_sqlite_path(rest),
        }),
        other => Err(format!(
            "unsupported DATABASE_URL scheme `{other}`; expected postgres / mysql / sqlite"
        )),
    }
}

/// Parse the `[user[:pass]@]host[:port][/db][?query]` body that the
/// postgres and mysql URLs share.
fn parse_userinfo_host_db<F, T>(rest: &str, build: F) -> T
where
    F: FnOnce(Option<String>, Option<u16>, Option<String>, Option<String>, Option<String>) -> T,
{
    // Drop any `?query`: the CLI clients do not take URL options.
    let body = rest.split_once('?').map_or(rest, |(b, _)| b);
    // Take the database name off the end.
    let (auth_host, database) = match body.split_once('/') {
        Some((auth_host, db)) if !db.is_empty() => (auth_host, Some(db.to_owned())),
        Some((auth_host, _)) => (auth_host, None),
        None => (body, None),
    };
    // Split auth from host on the last '@', so an '@' in the password
    // does not break it.
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
    build(host, port, user, password, database)
}

fn parse_sqlite_path(rest: &str) -> String {
    // sqlx accepts `sqlite::memory:`, `sqlite:///abs/path` and
    // `sqlite://path`. Once the scheme is stripped, what is left is
    // already the path `sqlite3` wants, so pass it through. The leading
    // `/` of the absolute form is part of the path and must stay.
    if rest == ":memory:" || rest.starts_with(":memory:") {
        return ":memory:".to_owned();
    }
    rest.to_owned()
}

/// Build the call for `target` as `(program, args, env_vars)`.
///
/// The password never goes in `args`, because argv is readable by any
/// user running `ps aux`. It goes in `env_vars` instead: `PGPASSWORD`
/// for `psql`, `MYSQL_PWD` for `mysql`. SQLite has no auth, so it gets
/// neither.
#[must_use]
pub fn command_for(target: &DbTarget) -> (&'static str, Vec<OsString>, Vec<(String, String)>) {
    match target {
        DbTarget::Postgres {
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
                args.push("-p".into());
                args.push(p.to_string().into());
            }
            if let Some(u) = user {
                args.push("-U".into());
                args.push(u.into());
            }
            if let Some(db) = database {
                args.push("-d".into());
                args.push(db.into());
            }
            let env = password
                .as_ref()
                .map(|pw| vec![("PGPASSWORD".to_owned(), pw.clone())])
                .unwrap_or_default();
            ("psql", args, env)
        }
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
            if let Some(db) = database {
                args.push(db.into());
            }
            let env = password
                .as_ref()
                .map(|pw| vec![("MYSQL_PWD".to_owned(), pw.clone())])
                .unwrap_or_default();
            ("mysql", args, env)
        }
        DbTarget::Sqlite { path } => ("sqlite3", vec![OsString::from(path)], Vec::new()),
    }
}

/// Start the right CLI for `url` and hand the process over to it. On
/// success this never returns. Unix uses `exec()`; other targets run
/// the child and pass its exit code on.
///
/// # Errors
/// When the URL cannot be parsed, and on non-Unix targets when the
/// child fails to start or exits non-zero.
pub fn run(url: &str) -> Result<std::convert::Infallible, Box<dyn std::error::Error>> {
    let target = parse_target(url).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let (program, args, env) = command_for(&target);

    let mut cmd = Command::new(program);
    cmd.args(&args);
    for (k, v) in &env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = cmd.exec();
        // `exec` returns only when it fails, e.g. binary not on PATH.
        Err(format!(
            "failed to exec `{program}` for dbshell: {err}. \
             Is the {program} client installed and on PATH?"
        )
        .into())
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        if status.success() {
            std::process::exit(0);
        }
        Err(format!(
            "`{program}` exited with status {status}. Is the {program} client installed and on PATH?"
        )
        .into())
    }
}

/// A libpq connection: the URL for argv, the password for `PGPASSWORD`,
/// so it never shows in `ps` (#1864, #2324).
pub(crate) struct LibpqConn {
    url: String,
    password: Option<String>,
}

impl LibpqConn {
    pub(crate) fn new(url: &str) -> Self {
        use crate::url_codec::percent_decode_path as decode;
        let mut password = None;
        let url = match crate::sql::connect_diagnosis::split_userinfo(url) {
            Some((scheme, userinfo, host)) => match userinfo.split_once(':') {
                Some((user, pw)) => {
                    password = Some(decode(pw));
                    format!("{scheme}://{user}@{host}")
                }
                None => url.to_owned(),
            },
            None => url.to_owned(),
        };
        // libpq also reads `?password=`.
        let (base, query) = url.split_once('?').unwrap_or((&url, ""));
        let kept: Vec<&str> = query
            .split('&')
            .filter(|kv| match kv.split_once('=') {
                Some((k, v)) if decode(k) == "password" => {
                    password = Some(decode(v));
                    false
                }
                _ => !kv.is_empty(),
            })
            .collect();
        let url = if kept.is_empty() {
            base.to_owned()
        } else {
            format!("{base}?{}", kept.join("&"))
        };
        Self { url, password }
    }

    /// The same server and credentials, database `db`.
    #[cfg_attr(not(all(feature = "tenancy", feature = "postgres")), allow(dead_code))]
    pub(crate) fn database(&self, db: &str) -> Self {
        let (base, query) = self
            .url
            .split_once('?')
            .map_or((&*self.url, None), |(b, q)| (b, Some(q)));
        let (scheme, rest) = base.split_once("://").unwrap_or(("postgres", base));
        let host = rest.split_once('/').map_or(rest, |(h, _)| h);
        let mut url = format!("{scheme}://{host}/{db}");
        if let Some(q) = query {
            url = format!("{url}?{q}");
        }
        Self {
            url,
            password: self.password.clone(),
        }
    }

    /// `program --dbname <url>`, the password in its environment.
    pub(crate) fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        if let Some(p) = &self.password {
            cmd.env("PGPASSWORD", p);
        }
        cmd.arg("--dbname").arg(&self.url);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1864 — the password goes to `PGPASSWORD`, never argv.
    #[test]
    fn conn_keeps_the_password_out_of_the_url() {
        let c = LibpqConn::new("postgres://al:p%40ss@w/rd@h:5432/app?sslmode=require");
        assert_eq!(c.url, "postgres://al@h:5432/app?sslmode=require");
        assert_eq!(c.password.as_deref(), Some("p@ss@w/rd"));
        let c = LibpqConn::new("postgres://al@h/app?password=s%3Dx&sslmode=disable");
        assert_eq!(c.url, "postgres://al@h/app?sslmode=disable");
        assert_eq!(c.password.as_deref(), Some("s=x"));
        let c = LibpqConn::new("postgres://h/app").database("stage");
        assert_eq!((c.url.as_str(), c.password), ("postgres://h/stage", None));
        let c = LibpqConn::new("postgres://al:pw@h/app?sslmode=require").database("stage");
        assert_eq!(c.url, "postgres://al@h/stage?sslmode=require");
        let argv: Vec<_> = c.command("psql").get_args().map(|a| a.to_owned()).collect();
        assert!(!argv.iter().any(|a| a.to_string_lossy().contains("pw")));
    }

    #[test]
    fn parse_postgres_full_dsn_into_components() {
        assert_eq!(
            parse_target("postgres://alice:secret@db.example.com:5432/myapp").unwrap(),
            DbTarget::Postgres {
                host: Some("db.example.com".to_owned()),
                port: Some(5432),
                user: Some("alice".to_owned()),
                password: Some("secret".to_owned()),
                database: Some("myapp".to_owned()),
            }
        );
    }

    #[test]
    fn parse_postgresql_alias_routes_to_postgres() {
        assert_eq!(
            parse_target("postgresql://localhost/dbname").unwrap(),
            DbTarget::Postgres {
                host: Some("localhost".to_owned()),
                port: None,
                user: None,
                password: None,
                database: Some("dbname".to_owned()),
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
        // sqlx options like `?ssl-mode=REQUIRED` mean nothing to the
        // mysql CLI, so they are dropped.
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

    /// The echoed scheme stops at the first `:`, before any userinfo.
    #[test]
    fn an_unknown_scheme_error_never_shows_userinfo() {
        let err = parse_target("x:pw@h/db?a=b://c").unwrap_err();
        assert!(err.contains("`x`"), "got: {err}");
        assert!(!err.contains("pw"), "got: {err}");
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

    /// Turn args into plain strings so tests can search them.
    fn args_str(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|s| s.into_string().unwrap()).collect()
    }

    #[test]
    fn command_for_postgres_emits_structured_flags() {
        let target = DbTarget::Postgres {
            host: Some("db.example.com".to_owned()),
            port: Some(5432),
            user: Some("alice".to_owned()),
            password: Some("secret".to_owned()),
            database: Some("myapp".to_owned()),
        };
        let (prog, args, env) = command_for(&target);
        assert_eq!(prog, "psql");
        assert_eq!(
            args_str(args),
            vec![
                "-h",
                "db.example.com",
                "-p",
                "5432",
                "-U",
                "alice",
                "-d",
                "myapp",
            ]
        );
        assert_eq!(env, vec![("PGPASSWORD".to_owned(), "secret".to_owned())]);
    }

    #[test]
    fn command_for_postgres_no_password_emits_no_env_var() {
        let target = DbTarget::Postgres {
            host: Some("localhost".to_owned()),
            port: None,
            user: None,
            password: None,
            database: None,
        };
        let (_, _, env) = command_for(&target);
        assert!(env.is_empty(), "no password → no PGPASSWORD env: {env:?}");
    }

    #[test]
    fn command_for_postgres_password_never_appears_in_args() {
        // A password in argv is visible to anyone running `ps aux`, so
        // pin that no argument ever holds it.
        let target = DbTarget::Postgres {
            host: Some("h".to_owned()),
            port: None,
            user: Some("u".to_owned()),
            password: Some("SUPER_SECRET_PASSWORD".to_owned()),
            database: Some("d".to_owned()),
        };
        let (_, args, env) = command_for(&target);
        let argv = args_str(args);
        assert!(
            !argv.iter().any(|a| a.contains("SUPER_SECRET_PASSWORD")),
            "password leaked into argv: {argv:?}"
        );
        // The env var may carry it; that is the safe path.
        assert!(env
            .iter()
            .any(|(k, v)| k == "PGPASSWORD" && v == "SUPER_SECRET_PASSWORD"));
    }

    #[test]
    fn command_for_mysql_emits_structured_flags() {
        let target = DbTarget::Mysql {
            host: Some("db.example.com".to_owned()),
            port: Some(3307),
            user: Some("alice".to_owned()),
            password: Some("secret".to_owned()),
            database: Some("myapp".to_owned()),
        };
        let (prog, args, env) = command_for(&target);
        assert_eq!(prog, "mysql");
        assert_eq!(
            args_str(args),
            vec!["-h", "db.example.com", "-P", "3307", "-u", "alice", "myapp",]
        );
        assert_eq!(env, vec![("MYSQL_PWD".to_owned(), "secret".to_owned())]);
    }

    #[test]
    fn command_for_mysql_password_never_appears_in_args() {
        // Same as postgres: no `--password=...` or `-p<pass>` in argv.
        let target = DbTarget::Mysql {
            host: Some("h".to_owned()),
            port: None,
            user: Some("u".to_owned()),
            password: Some("SUPER_SECRET_PASSWORD".to_owned()),
            database: None,
        };
        let (_, args, env) = command_for(&target);
        let argv = args_str(args);
        assert!(
            !argv.iter().any(|a| a.contains("SUPER_SECRET_PASSWORD")),
            "password leaked into argv: {argv:?}"
        );
        assert!(env
            .iter()
            .any(|(k, v)| k == "MYSQL_PWD" && v == "SUPER_SECRET_PASSWORD"));
    }

    #[test]
    fn command_for_sqlite_uses_sqlite3_with_path() {
        let target = DbTarget::Sqlite {
            path: ":memory:".to_owned(),
        };
        let (prog, args, env) = command_for(&target);
        assert_eq!(prog, "sqlite3");
        assert_eq!(args, vec![OsString::from(":memory:")]);
        assert!(env.is_empty(), "sqlite has no auth, no env var");
    }
}
