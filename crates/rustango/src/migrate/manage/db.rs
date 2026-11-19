//! `db:dump`, `db:restore` and `db:info`.

use super::*;

// =====================================================================
// db:dump / db:restore — shell out to pg_dump / psql
// =====================================================================

#[derive(Debug, PartialEq)]
pub(super) struct DbDumpArgs {
    out: Option<String>,
    data_only: bool,
    schema_only: bool,
    no_owner: bool,
}

pub(super) fn parse_db_dump_args(args: &[String]) -> Result<DbDumpArgs, MigrateError> {
    let mut out: Option<String> = None;
    let mut data_only = false;
    let mut schema_only = false;
    let mut no_owner = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--out" | "-o" => {
                out = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| MigrateError::Validation("--out requires a path".into()))?,
                );
            }
            "--data-only" => data_only = true,
            "--schema-only" => schema_only = true,
            "--no-owner" => no_owner = true,
            other => {
                return Err(unknown_flag(other));
            }
        }
    }
    if data_only && schema_only {
        return Err(MigrateError::Validation(
            "--data-only and --schema-only are mutually exclusive".into(),
        ));
    }
    Ok(DbDumpArgs {
        out,
        data_only,
        schema_only,
        no_owner,
    })
}

/// `pg_dump` for `url`; the password goes in its env, not argv (#2324).
pub(super) fn pg_dump_command(
    parsed: &DbDumpArgs,
    url: &str,
) -> Result<std::process::Command, MigrateError> {
    let mut cmd = crate::dbshell::LibpqConn::new(url)
        .map_err(MigrateError::Validation)?
        .command("pg_dump");
    cmd.args(build_pg_dump_argv(parsed));
    Ok(cmd)
}

/// pg_dump's flags, after `--dbname`.
pub(super) fn build_pg_dump_argv(parsed: &DbDumpArgs) -> Vec<String> {
    let mut argv = Vec::new();
    if parsed.data_only {
        argv.push("--data-only".into());
    }
    if parsed.schema_only {
        argv.push("--schema-only".into());
    }
    if parsed.no_owner {
        argv.push("--no-owner".into());
    }
    if let Some(out) = &parsed.out {
        argv.push("--file".into());
        argv.push(out.clone());
    }
    argv
}

/// Takes no writer, on purpose. Without `--out`, `pg_dump` inherits our
/// stdout and *is* the output — so anything we print there lands inside
/// the user's `.sql` file. `db:dump > backup.sql` is the documented
/// form, and it used to produce a file whose first line was a status
/// banner (#1404). The banner goes to stderr, and there is now no
/// writer here to put it anywhere else.
pub(super) fn db_dump_cmd(args: &[String]) -> Result<(), MigrateError> {
    let parsed = parse_db_dump_args(args)?;
    let url = crate::env::database_url_from_env()
        .map_err(|e| MigrateError::Validation(format!("db:dump needs a database: {e}")))?;
    let mut cmd = pg_dump_command(&parsed, &url)?;
    eprintln!("running: pg_dump {}", shown_args(&cmd));
    let status = cmd.status().map_err(|e| {
        MigrateError::Validation(format!("could not run pg_dump (is it on PATH?): {e}"))
    })?;
    if !status.success() {
        return Err(MigrateError::Validation(format!(
            "pg_dump exited with status {status}"
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub(super) struct DbRestoreArgs {
    file: String,
    clean: bool,
    yes: bool,
}

pub(super) fn parse_db_restore_args(args: &[String]) -> Result<DbRestoreArgs, MigrateError> {
    let mut file: Option<String> = None;
    let mut clean = false;
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--clean" => clean = true,
            "--yes" => yes = true,
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                if file.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected argument: {other}"
                    )));
                }
                file = Some(other.to_owned());
            }
        }
    }
    let file = file.ok_or_else(|| {
        MigrateError::Validation("db:restore <file> requires a dump file path".into())
    })?;
    Ok(DbRestoreArgs { file, clean, yes })
}

/// A restore that may run: the dump is a readable file and any
/// `--clean` was confirmed. Only [`RestorePlan::check`] builds one (#2283).
#[derive(Debug)]
pub(super) struct RestorePlan {
    file: String,
    clean: bool,
}

impl RestorePlan {
    /// `confirm` asks the user when `--clean` came without `--yes`.
    fn check(
        parsed: DbRestoreArgs,
        confirm: impl FnOnce(&str) -> bool,
    ) -> Result<Self, MigrateError> {
        // Metadata only: opening a FIFO with no writer blocks. A pipe is fine
        // for a plain load; `--clean` commits its DROP even for an empty dump.
        let usable = std::fs::metadata(&parsed.file)
            .map_err(|e| e.to_string())
            .and_then(|m| {
                if !parsed.clean {
                    Ok(())
                } else if !m.is_file() {
                    Err("--clean needs a regular file".to_owned())
                } else if m.len() == 0 {
                    Err("the file is empty".to_owned())
                } else {
                    Ok(())
                }
            });
        if let Err(e) = usable {
            return Err(MigrateError::Validation(format!(
                "db:restore: cannot read `{}`: {e}",
                parsed.file
            )));
        }
        if parsed.clean && !parsed.yes && !confirm(&parsed.file) {
            return Err(MigrateError::Validation(
                "db:restore --clean drops schema `public` first: pass --yes to confirm".into(),
            ));
        }
        Ok(Self {
            file: parsed.file,
            clean: parsed.clean,
        })
    }
}

/// Interactive yes for `--clean`; `false` off a terminal.
pub(super) fn confirm_clean_restore(file: &str) -> bool {
    #[cfg(any(feature = "admin", feature = "tenancy"))]
    {
        let prompt =
            format!("db:restore --clean drops schema `public`, then loads {file}. Type `yes`: ");
        answered_yes(crate::manage_interactive::ask(&prompt))
    }
    #[cfg(not(any(feature = "admin", feature = "tenancy")))]
    {
        let _ = file;
        false
    }
}

/// Only a typed `yes` confirms; no terminal, EOF or anything else refuses.
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(super) fn answered_yes(answer: std::io::Result<Option<String>>) -> bool {
    matches!(answer, Ok(Some(a)) if a == "yes")
}

/// `psql` for `url`; the password goes in its env, not argv (#2324).
pub(super) fn psql_restore_command(
    psql: &str,
    parsed: &RestorePlan,
    url: &str,
) -> Result<std::process::Command, MigrateError> {
    let mut cmd = crate::dbshell::LibpqConn::new(url)
        .map_err(MigrateError::Validation)?
        .command(psql);
    cmd.args(build_psql_argv(parsed));
    Ok(cmd)
}

/// psql's flags, after `--dbname`.
pub(super) fn build_psql_argv(parsed: &RestorePlan) -> Vec<String> {
    let mut argv = Vec::new();
    // -v ON_ERROR_STOP=1 makes psql exit non-zero on the first SQL
    // error, instead of plowing through and "succeeding" with garbage.
    argv.push("-v".into());
    argv.push("ON_ERROR_STOP=1".into());
    // One transaction around the DROP and the load: a failed load rolls the DROP back (#2283).
    argv.push("--single-transaction".into());
    if parsed.clean {
        argv.push("-c".into());
        argv.push("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;".into());
    }
    argv.push("-f".into());
    argv.push(parsed.file.clone());
    argv
}

pub(super) fn db_restore_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let parsed = RestorePlan::check(parse_db_restore_args(args)?, confirm_clean_restore)?;
    let url = crate::env::database_url_from_env()
        .map_err(|e| MigrateError::Validation(format!("db:restore needs a database: {e}")))?;
    run_psql_restore("psql", &parsed, &url, w)
}

/// Split from [`db_restore_cmd`] so a test can aim it at a private database
/// and a `psql` off `PATH`.
pub(super) fn run_psql_restore<W: Write>(
    psql: &str,
    parsed: &RestorePlan,
    url: &str,
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut cmd = psql_restore_command(psql, parsed, url)?;
    writeln!(w, "running: psql {}", shown_args(&cmd))?;
    let status = cmd.status().map_err(|e| {
        MigrateError::Validation(format!("could not run psql (is it on PATH?): {e}"))
    })?;
    if !status.success() {
        return Err(MigrateError::Validation(format!(
            "psql exited with status {status}"
        )));
    }
    Ok(())
}

/// `db:info` — read-only summary of the DB configuration this build
/// would use. Shows the resolved URL (password redacted), the detected
/// backend, and which backend Cargo features are compiled in. Does not
/// connect — handy in CI / containers where DB might not be reachable
/// yet but you still want to confirm the runtime sees the expected
/// backend.
pub(super) fn db_info_cmd<W: Write>(w: &mut W) -> Result<(), MigrateError> {
    write_db_info(w, crate::env::database_url_from_env())
}

pub(super) fn write_db_info<W: Write>(
    w: &mut W,
    url: Result<String, crate::env::EnvError>,
) -> Result<(), MigrateError> {
    writeln!(w, "rustango db:info")?;
    writeln!(w, "  framework version:  {}", env!("CARGO_PKG_VERSION"))?;

    let backends = [
        ("postgres", cfg!(feature = "postgres")),
        ("mysql", cfg!(feature = "mysql")),
        ("sqlite", cfg!(feature = "sqlite")),
    ];
    for (feature, on) in backends {
        let label = format!("{feature} feature:");
        writeln!(
            w,
            "  {label:<20}{}",
            if on { "enabled" } else { "disabled" }
        )?;
    }

    match url {
        Ok(url) => {
            // Up to the first colon, as `Pool::connect` reads it:
            // SQLite URLs are often `sqlite:path` with no `//`.
            let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
            writeln!(w, "  resolved URL:       {}", redact_url(&url))?;
            writeln!(w, "  detected backend:   {scheme}")?;
            // Caught at runtime by Pool::connect too; this surfaces it
            // before the operator tries to start the server.
            let feature = match scheme.as_str() {
                "postgres" | "postgresql" => Some("postgres"),
                "mysql" => Some("mysql"),
                "sqlite" => Some("sqlite"),
                _ => None,
            };
            if let Some((feature, false)) =
                feature.and_then(|f| backends.into_iter().find(|(b, _)| *b == f))
            {
                writeln!(
                    w,
                    "  ! warning: URL is {feature} but the `{feature}` feature is disabled — \
                     add `features = [\"{feature}\"]` to rustango"
                )?;
            }
        }
        Err(e) => {
            writeln!(w, "  resolved URL:       (none — {e})")?;
            writeln!(
                w,
                "  hint:               set DATABASE_URL or DB_USER+DB_NAME (+optional \
                 DB_HOST/DB_PORT/DB_PASSWORD/DB_DRIVER/DB_PARAMS)"
            )?;
        }
    }
    Ok(())
}

/// A command's argv for a status line; the URL in it has no password.
pub(super) fn shown_args(cmd: &std::process::Command) -> String {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod db_cmd_tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    // -------- parse_db_dump_args

    #[test]
    fn dump_no_flags_defaults() {
        let p = parse_db_dump_args(&[]).unwrap();
        assert!(p.out.is_none());
        assert!(!p.data_only);
        assert!(!p.schema_only);
        assert!(!p.no_owner);
    }

    #[test]
    fn dump_out_flag_with_value() {
        let p = parse_db_dump_args(&args(&["--out", "/tmp/db.sql"])).unwrap();
        assert_eq!(p.out.as_deref(), Some("/tmp/db.sql"));
    }

    #[test]
    fn dump_short_o_flag() {
        let p = parse_db_dump_args(&args(&["-o", "/tmp/db.sql"])).unwrap();
        assert_eq!(p.out.as_deref(), Some("/tmp/db.sql"));
    }

    #[test]
    fn dump_data_only_flag() {
        let p = parse_db_dump_args(&args(&["--data-only"])).unwrap();
        assert!(p.data_only);
        assert!(!p.schema_only);
    }

    #[test]
    fn dump_schema_only_flag() {
        let p = parse_db_dump_args(&args(&["--schema-only"])).unwrap();
        assert!(p.schema_only);
        assert!(!p.data_only);
    }

    #[test]
    fn dump_no_owner_flag() {
        let p = parse_db_dump_args(&args(&["--no-owner"])).unwrap();
        assert!(p.no_owner);
    }

    #[test]
    fn dump_out_without_value_errors() {
        let r = parse_db_dump_args(&args(&["--out"]));
        assert!(r.is_err());
    }

    #[test]
    fn dump_data_and_schema_only_conflict() {
        let r = parse_db_dump_args(&args(&["--data-only", "--schema-only"]));
        assert!(r.is_err());
    }

    #[test]
    fn dump_unknown_flag_errors() {
        let r = parse_db_dump_args(&args(&["--bogus"]));
        assert!(r.is_err());
    }

    // -------- build_pg_dump_argv

    #[test]
    fn dump_argv_starts_with_a_password_free_dbname() {
        let parsed = DbDumpArgs {
            out: None,
            data_only: false,
            schema_only: false,
            no_owner: false,
        };
        let cmd = pg_dump_command(&parsed, "postgres://u:s3cret@h/db").unwrap();
        let argv: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(argv[..2], ["--dbname", "postgres://u@h/db"]);
    }

    /// #2324: argv is visible in `ps`; the password goes in the env.
    #[test]
    fn dump_and_restore_keep_the_password_out_of_argv() {
        let url = "postgres://u:s3cret@h/db";
        let dump = DbDumpArgs {
            out: None,
            data_only: false,
            schema_only: false,
            no_owner: false,
        };
        let plan = RestorePlan {
            file: "/tmp/x.sql".into(),
            clean: false,
        };
        for cmd in [
            pg_dump_command(&dump, url).unwrap(),
            psql_restore_command("psql", &plan, url).unwrap(),
        ] {
            assert!(
                !cmd.get_args()
                    .any(|a| a.to_string_lossy().contains("s3cret")),
                "{cmd:?}"
            );
            let env: Vec<_> = cmd.get_envs().collect();
            assert_eq!(
                env,
                [(
                    std::ffi::OsStr::new("PGPASSWORD"),
                    Some(std::ffi::OsStr::new("s3cret"))
                )]
            );
        }
    }

    #[test]
    fn dump_argv_includes_chosen_flags() {
        let parsed = DbDumpArgs {
            out: Some("/tmp/x.sql".into()),
            data_only: true,
            schema_only: false,
            no_owner: true,
        };
        let argv = build_pg_dump_argv(&parsed);
        assert!(argv.contains(&"--data-only".to_owned()));
        assert!(argv.contains(&"--no-owner".to_owned()));
        assert!(argv.contains(&"--file".to_owned()));
        assert!(argv.contains(&"/tmp/x.sql".to_owned()));
        assert!(!argv.contains(&"--schema-only".to_owned()));
    }

    // -------- parse_db_restore_args

    #[test]
    fn restore_requires_file() {
        let r = parse_db_restore_args(&[]);
        assert!(r.is_err());
    }

    #[test]
    fn restore_positional_file() {
        let p = parse_db_restore_args(&args(&["/tmp/db.sql"])).unwrap();
        assert_eq!(p.file, "/tmp/db.sql");
        assert!(!p.clean);
    }

    #[test]
    fn restore_with_clean_flag() {
        let p = parse_db_restore_args(&args(&["--clean", "/tmp/db.sql"])).unwrap();
        assert!(p.clean);
        assert_eq!(p.file, "/tmp/db.sql");
    }

    #[test]
    fn restore_clean_after_file() {
        let p = parse_db_restore_args(&args(&["/tmp/db.sql", "--clean"])).unwrap();
        assert!(p.clean);
    }

    #[test]
    fn restore_two_files_errors() {
        let r = parse_db_restore_args(&args(&["a.sql", "b.sql"]));
        assert!(r.is_err());
    }

    // -------- RestorePlan::check (#2283)

    fn restore_args(file: &str, clean: bool, yes: bool) -> DbRestoreArgs {
        DbRestoreArgs {
            file: file.into(),
            clean,
            yes,
        }
    }

    #[test]
    fn restore_refuses_a_missing_file_before_asking() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.sql");
        let missing = missing.to_str().unwrap();
        let err = RestorePlan::check(restore_args(missing, true, false), |_| {
            panic!("asked to confirm a file that does not exist")
        })
        .unwrap_err();
        assert!(err.to_string().contains("cannot read"), "{err}");
        let dir_path = dir.path().to_str().unwrap();
        assert!(RestorePlan::check(restore_args(dir_path, true, true), |_| true).is_err());
    }

    #[test]
    fn restore_clean_needs_yes_or_a_confirm() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "SELECT 1;\n").unwrap();
        let path = f.path().to_str().unwrap();
        let err = RestorePlan::check(restore_args(path, true, false), |_| false).unwrap_err();
        assert!(err.to_string().contains("--yes"), "{err}");
        assert!(RestorePlan::check(restore_args(path, true, false), |_| true).is_ok());
        assert!(RestorePlan::check(restore_args(path, true, true), |_| false).is_ok());
        assert!(RestorePlan::check(restore_args(path, false, false), |_| false).is_ok());
        let p = parse_db_restore_args(&args(&["--clean", "--yes", path])).unwrap();
        assert!(p.clean && p.yes);
    }

    /// An empty dump under `--clean` would commit the DROP and load nothing.
    #[test]
    fn restore_clean_refuses_an_empty_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_str().unwrap();
        let err = RestorePlan::check(restore_args(path, true, true), |_| true).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
        assert!(RestorePlan::check(restore_args(path, false, false), |_| false).is_ok());
    }

    /// A plain load still takes a pipe; `--clean` does not.
    #[cfg(unix)]
    #[test]
    fn restore_takes_a_pipe_only_without_clean() {
        assert!(RestorePlan::check(restore_args("/dev/stdin", false, false), |_| false).is_ok());
        let err = RestorePlan::check(restore_args("/dev/stdin", true, true), |_| true).unwrap_err();
        assert!(err.to_string().contains("regular file"), "{err}");
    }

    #[test]
    fn restore_prompt_needs_a_typed_yes() {
        assert!(answered_yes(Ok(Some("yes".into()))));
        for no in [Ok(Some("y".into())), Ok(Some("YES".into())), Ok(None)] {
            assert!(!answered_yes(no));
        }
        assert!(!answered_yes(Err(std::io::Error::other("tty"))));
    }

    /// `--clean` with a failing dump rolls the DROP back. Runs on a database
    /// it creates and drops, never the shared one (#2283).
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn restore_clean_rolls_back_a_bad_dump_on_a_private_database() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set — skipping");
            return;
        };
        // `$RUSTANGO_TEST_PSQL`, else `psql` on PATH; CI must have one.
        let runs = |p: &str| {
            std::process::Command::new(p)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let psql = std::env::var("RUSTANGO_TEST_PSQL").unwrap_or_else(|_| "psql".to_owned());
        if !runs(&psql) {
            assert!(
                std::env::var_os("CI").is_none(),
                "db:restore needs `psql` on PATH in CI"
            );
            eprintln!("SKIPPED: `{psql}` does not run (set RUSTANGO_TEST_PSQL); db:restore is unproven here");
            return;
        }
        let db = format!("rustango_restore_{}", std::process::id());
        let (base, _) = url.rsplit_once('/').expect("DATABASE_URL names a database");
        let private = format!("{base}/{db}");
        let admin = sqlx::PgPool::connect(&url).await.expect("connect");
        let drop_db = format!(r#"DROP DATABASE IF EXISTS "{db}" WITH (FORCE)"#);
        sqlx::query(&drop_db).execute(&admin).await.unwrap();
        sqlx::query(&format!(r#"CREATE DATABASE "{db}""#))
            .execute(&admin)
            .await
            .unwrap();
        let p = sqlx::PgPool::connect(&private).await.expect("private db");
        for sql in ["CREATE TABLE keep (id int)", "INSERT INTO keep VALUES (1)"] {
            sqlx::query(sql).execute(&p).await.unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.sql");
        std::fs::write(
            &bad,
            "CREATE TABLE half (id int);\nSELECT * FROM no_such_table;\n",
        )
        .unwrap();

        let plan = RestorePlan::check(restore_args(bad.to_str().unwrap(), true, true), |_| true)
            .expect("plan");
        let res = run_psql_restore(&psql, &plan, &private, &mut Vec::new());
        let kept: Result<i64, _> = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM keep")
            .fetch_one(&p)
            .await;
        p.close().await;
        sqlx::query(&drop_db).execute(&admin).await.unwrap();
        assert_eq!(kept.ok(), Some(1), "the bad dump left the DROP committed");
        assert!(res.unwrap_err().to_string().contains("psql exited"));
    }

    // -------- build_psql_argv

    #[test]
    fn restore_argv_includes_on_error_stop() {
        let parsed = RestorePlan {
            file: "/tmp/x.sql".into(),
            clean: false,
        };
        let argv = build_psql_argv(&parsed);
        // ON_ERROR_STOP=1 prevents psql from continuing past errors
        // and silently "succeeding" with a half-restored DB.
        assert!(argv.contains(&"ON_ERROR_STOP=1".to_owned()));
        assert!(argv.contains(&"--single-transaction".to_owned()));
        assert!(argv.contains(&"-f".to_owned()));
        assert!(argv.contains(&"/tmp/x.sql".to_owned()));
        assert!(!argv.iter().any(|a| a.contains("DROP SCHEMA")));
    }

    #[test]
    fn restore_argv_with_clean_drops_schema() {
        let parsed = RestorePlan {
            file: "/tmp/x.sql".into(),
            clean: true,
        };
        let argv = build_psql_argv(&parsed);
        assert!(argv.iter().any(|a| a.contains("DROP SCHEMA")));
        assert!(argv.iter().any(|a| a.contains("CREATE SCHEMA")));
    }

    // -------- redact_url

    #[test]
    fn redact_masks_password_in_postgres_url() {
        assert_eq!(
            redact_url("postgres://alice:supersecret@localhost:5432/mydb"),
            "postgres://alice:***@localhost:5432/mydb"
        );
    }

    #[test]
    fn redact_passes_through_url_without_credentials() {
        assert_eq!(
            redact_url("postgres://localhost:5432/mydb"),
            "postgres://localhost:5432/mydb"
        );
    }

    #[test]
    fn redact_passes_through_non_urls() {
        assert_eq!(redact_url("--data-only"), "--data-only");
        assert_eq!(redact_url("/tmp/db.sql"), "/tmp/db.sql");
    }

    #[test]
    fn redact_handles_url_with_only_user() {
        // No colon → no password to redact → pass through.
        assert_eq!(
            redact_url("postgres://alice@localhost/db"),
            "postgres://alice@localhost/db"
        );
    }

    #[test]
    fn about_masks_a_query_password() {
        assert_eq!(
            about_db_url(Some("postgres://db/app?password=x".into())),
            "postgres://db/app?password=***"
        );
        assert_eq!(about_db_url(None), "(unset)");
    }

    #[test]
    fn redact_masks_a_password_holding_an_at_sign() {
        assert_eq!(
            redact_url("postgres://alice:p@ss@localhost/db"),
            "postgres://alice:***@localhost/db"
        );
    }

    /// Every backend gets a feature line and a warning when it is off;
    /// a working MySQL setup gets no note.
    #[test]
    fn db_info_reports_each_backend_feature() {
        let info = |url: &str| {
            let mut buf = Vec::new();
            write_db_info(&mut buf, Ok(url.to_owned())).unwrap();
            String::from_utf8(buf).unwrap()
        };
        for (url, feature, on) in [
            ("postgres://u@h/db", "postgres", cfg!(feature = "postgres")),
            ("mysql://u@h/db", "mysql", cfg!(feature = "mysql")),
            ("sqlite::memory:", "sqlite", cfg!(feature = "sqlite")),
        ] {
            let out = info(url);
            let state = if on { "enabled" } else { "disabled" };
            let line = format!("{:<20}{state}", format!("{feature} feature:"));
            assert!(out.contains(&line), "{out}");
            assert!(
                out.contains(&format!("detected backend:   {feature}")),
                "{out}"
            );
            assert_eq!(out.contains("! warning"), !on, "{out}");
            assert!(!out.contains("! note"), "{out}");
        }
    }

    /// A cache verb `--help` lists must not answer "unknown subcommand".
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn cache_verbs_follow_the_cache_feature() {
        let mut help = Vec::new();
        print_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert_eq!(help.contains("clear-cache"), cfg!(feature = "cache"));
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        for verb in ["clear-cache", "createcachetable"] {
            let mut out = Vec::new();
            let r =
                run_with_writer(&pool, Path::new("/nonexistent"), args(&[verb]), &mut out).await;
            if cfg!(feature = "cache") {
                assert!(r.is_ok(), "{verb}: {r:?}");
            } else {
                let err = r.unwrap_err().to_string();
                assert!(err.contains("needs the `cache` feature"), "{verb}: {err}");
            }
        }
    }
}
