//! `migrate-tenant-storage <slug> --to schema|database` — flip a
//! populated tenant between `schema` and `database` storage modes
//! safely. Closes the gap that locks `storage_mode` in the operator
//! console (`op_orgs_edit.html` LOCKED_ORG_FIELDS) — without this
//! verb, an operator who flips the field via raw SQL ends up with
//! the data sitting at the OLD location and no resolver pointing at
//! it (split-brain).
//!
//! **Postgres-only by language**: this whole verb is gated on the
//! `postgres` feature because (a) `schema` storage mode requires
//! `SET search_path`, which is a PG-specific SQL statement; (b) the
//! data move uses `pg_dump | psql`, which is part of the PG client
//! tools and has no MySQL/SQLite analog with the same shape. Sqlite
//! and MySQL apps don't need this verb — they only have
//! `database` mode.
#![cfg(feature = "postgres")]
//!
//! ## Algorithm
//!
//! 1. Look up `Org` by slug. Validate target ≠ current storage_mode.
//! 2. Provision the target storage:
//!    - schema → the restore creates it; it must not exist yet. The
//!      registry user needs `CREATEDB` for a staging database.
//!    - database → caller passes `--database-url`. Database must
//!      already exist (we don't `CREATE DATABASE` — that's a
//!      single-statement decision the operator should own).
//! 3. `pg_dump` the source (schema-scoped or full DB), pipe into
//!    `psql` against the target. Into a schema it goes through a
//!    staging database that renames `public` first.
//! 4. Smoke check: `SELECT 1 FROM <schema>.rustango_users LIMIT 1`
//!    against the new location, before the Org row moves.
//! 5. Single-statement transaction: `UPDATE rustango_orgs SET
//!    storage_mode = ?, database_url = ?, schema_name = ? WHERE
//!    slug = ?`.
//! 6. `TenantPools::invalidate(slug)` so the next request rebuilds
//!    the cached pool against the new storage.
//!
//! Shells out to `pg_dump` and `psql` — operators must have both on
//! PATH. We considered an in-Rust dump implementation; pg_dump is
//! battle-tested for cross-version compat (extensions, sequences,
//! constraints, JSONB defaults) and shipping our own would be a
//! perpetual chase.
//!
//! ## What this verb does NOT do
//!
//! * It does NOT drop the source data after a successful move. The
//!   operator can `purge-tenant --purge-database` (database-mode
//!   source) or manually `DROP SCHEMA` (schema-mode source) when
//!   they're confident the new side is healthy.
//! * It does NOT create the target database. Database-mode targets
//!   need `createdb` / `CREATE DATABASE` to have run already.
//! * It does NOT verify the data row counts match between source
//!   and target — only that the new location is reachable. Strict
//!   parity is the operator's responsibility.

use std::io::Write;
use std::process::Stdio;

use crate::core::Column as _;
use crate::tenancy::error::TenancyError;
use crate::tenancy::manage::args::{next_value, quote_ident};
use crate::tenancy::org::{Org, StorageMode};
use crate::tenancy::pools::TenantPools;

/// Parsed `migrate-tenant-storage` arguments.
#[derive(Debug)]
struct MigrateStorageArgs {
    slug: String,
    target: StorageMode,
    /// Required when `target = Database`, ignored otherwise.
    database_url: Option<String>,
    /// Optional override for the target schema name (database → schema).
    /// Defaults to the slug.
    schema_name: Option<String>,
    dry_run: bool,
}

pub(super) async fn migrate_tenant_storage_cmd<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    args: &[String],
    writer: &mut W,
) -> Result<(), TenancyError> {
    let parsed = parse_args(args)?;

    // 1. Look up the Org row.
    let mut orgs: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(parsed.slug.clone()))
        .fetch_on(pools.registry())
        .await?;
    let mut org = orgs
        .pop()
        .ok_or_else(|| TenancyError::Validation(format!("tenant `{}` not found", parsed.slug)))?;

    let current = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{}` has unknown storage_mode `{got}`",
            parsed.slug
        ))
    })?;
    if current == parsed.target {
        return Err(TenancyError::Validation(format!(
            "tenant `{}` is already in `{}` mode — nothing to do",
            parsed.slug, parsed.target
        )));
    }
    if parsed.target == StorageMode::Database && parsed.database_url.is_none() {
        return Err(TenancyError::Validation(
            "--to database requires --database-url <conninfo>".into(),
        ));
    }

    // 2. Compute source / target connection details.
    let source_url = match current {
        StorageMode::Schema => registry_url.to_owned(),
        StorageMode::Database => org.database_url.clone().ok_or_else(|| {
            TenancyError::Validation(format!(
                "tenant `{}` has no database_url despite database mode",
                parsed.slug
            ))
        })?,
    };
    let source_schema = match current {
        StorageMode::Schema => Some(
            org.schema_name
                .clone()
                .unwrap_or_else(|| parsed.slug.clone()),
        ),
        StorageMode::Database => None,
    };
    let target_schema = match parsed.target {
        StorageMode::Schema => Some(
            parsed
                .schema_name
                .clone()
                .unwrap_or_else(|| parsed.slug.clone()),
        ),
        StorageMode::Database => None,
    };
    let target_url = match parsed.target {
        StorageMode::Schema => registry_url.to_owned(),
        StorageMode::Database => parsed.database_url.clone().expect("validated above"),
    };

    writeln!(
        writer,
        "migrate-tenant-storage `{}`: {} → {}",
        parsed.slug, current, parsed.target,
    )?;
    if let Some(s) = &source_schema {
        writeln!(
            writer,
            "  source: {} (schema `{s}`)",
            redact_url(&source_url)
        )?;
    } else {
        writeln!(writer, "  source: {}", redact_url(&source_url))?;
    }
    if let Some(s) = &target_schema {
        writeln!(
            writer,
            "  target: {} (schema `{s}`)",
            redact_url(&target_url)
        )?;
    } else {
        writeln!(writer, "  target: {}", redact_url(&target_url))?;
    }

    if parsed.dry_run {
        writeln!(writer, "  [dry-run] no changes — exit.")?;
        return Ok(());
    }

    // 3-4. Dump → restore. Streams pg_dump stdout into psql stdin so
    // we never buffer the full snapshot in memory.
    writeln!(writer, "  starting pg_dump → psql pipe…")?;
    let (source, target) = (Conn::new(&source_url), Conn::new(&target_url));
    match &target_schema {
        Some(schema) => restore_into_schema(pools.registry(), &source, &target, schema).await?,
        None => pg_dump_to_psql(&source, source_schema.as_deref(), &target)?,
    }
    writeln!(writer, "  data move OK")?;

    // Before the Org row moves, so a bad restore leaves it in place.
    smoke_check(&target_url, target_schema.as_deref())
        .await
        .map_err(|e| TenancyError::Validation(format!("smoke-check failed: {e}")))?;
    writeln!(writer, "  smoke-check OK")?;

    // 5. Update Org row.
    let new_storage_mode = parsed.target.as_str().into();
    let new_database_url = match parsed.target {
        StorageMode::Database => Some(target_url.clone()),
        StorageMode::Schema => None,
    };
    let new_schema_name = match parsed.target {
        StorageMode::Schema => target_schema.clone(),
        StorageMode::Database => None,
    };
    org.storage_mode = new_storage_mode;
    org.database_url = new_database_url;
    org.schema_name = new_schema_name;
    org.save(pools.registry()).await?;
    writeln!(writer, "  Org row updated")?;

    // 6. Evict the cached pool.
    pools.invalidate(&parsed.slug).await;
    writeln!(
        writer,
        "  running servers switch to the new location within {} s (their tenant cache TTL)",
        crate::tenancy::resolver::CACHE_TTL.as_secs()
    )?;
    writeln!(
        writer,
        "  ✓ migrated `{}` to {} mode. Source data still at the old location — `purge-tenant --purge-database` or DROP SCHEMA when ready.",
        parsed.slug, parsed.target,
    )?;
    Ok(())
}

fn parse_args(args: &[String]) -> Result<MigrateStorageArgs, TenancyError> {
    let mut iter = args.iter();
    let slug = iter
        .next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation("migrate-tenant-storage: missing <slug>".into()))?;
    let mut target: Option<StorageMode> = None;
    let mut database_url: Option<String> = None;
    let mut schema_name: Option<String> = None;
    let mut dry_run = false;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--to" => {
                let v = next_value(&mut iter, "--to")?;
                target = Some(StorageMode::parse(&v).map_err(|got| {
                    TenancyError::Validation(format!(
                        "--to must be `schema` or `database`, got `{got}`"
                    ))
                })?);
            }
            "--database-url" => database_url = Some(next_value(&mut iter, "--database-url")?),
            "--schema-name" => schema_name = Some(next_value(&mut iter, "--schema-name")?),
            "--dry-run" => dry_run = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "migrate-tenant-storage <slug> --to schema|database \
                     [--database-url <conninfo>] [--schema-name <s>] [--dry-run]"
                        .into(),
                ));
            }
            other => {
                return Err(TenancyError::Validation(format!(
                    "migrate-tenant-storage: unknown argument `{other}`"
                )));
            }
        }
    }
    let target = target.ok_or_else(|| {
        TenancyError::Validation("migrate-tenant-storage: --to schema|database is required".into())
    })?;
    Ok(MigrateStorageArgs {
        slug,
        target,
        database_url,
        schema_name,
        dry_run,
    })
}

/// A libpq connection: the URL for argv, the password for `PGPASSWORD`,
/// so it never shows in `ps` (#1864).
struct Conn {
    url: String,
    password: Option<String>,
}

impl Conn {
    fn new(url: &str) -> Self {
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
    fn database(&self, db: &str) -> Self {
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

    fn command(&self, program: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(program);
        if let Some(p) = &self.password {
            cmd.env("PGPASSWORD", p);
        }
        cmd.arg("--dbname").arg(&self.url);
        cmd
    }

    fn psql(&self) -> std::process::Command {
        let mut cmd = self.command("psql");
        cmd.args(["--quiet", "--no-psqlrc", "-v", "ON_ERROR_STOP=1"]);
        cmd
    }
}

/// Restore `source`'s `public` into `schema` on `target`. pg_dump names
/// every object `public.`, so `public` is renamed in a staging database
/// first (#1864).
async fn restore_into_schema(
    registry: &crate::sql::sqlx::PgPool,
    source: &Conn,
    target: &Conn,
    schema: &str,
) -> Result<(), TenancyError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let staging = format!("rustango_stage_{nanos}");
    let quoted = quote_ident(&staging);
    crate::sql::sqlx::query(&format!("CREATE DATABASE {quoted}"))
        .execute(registry)
        .await?;
    let stage = target.database(&staging);
    let moved = pg_dump_to_psql(source, None, &stage)
        .and_then(|()| {
            let mut rename = stage.psql();
            rename.arg("-c").arg(format!(
                "ALTER SCHEMA public RENAME TO {}",
                quote_ident(schema)
            ));
            run(rename, "psql rename")
        })
        .and_then(|()| pg_dump_to_psql(&stage, Some(schema), target));
    let dropped = crate::sql::sqlx::query(&format!("DROP DATABASE {quoted} WITH (FORCE)"))
        .execute(registry)
        .await;
    moved?;
    dropped?;
    Ok(())
}

/// Pipe `pg_dump <source>` into `psql <target>` in one transaction. When
/// the source is schema-scoped, pass `--schema=<name>` to pg_dump.
fn pg_dump_to_psql(
    source: &Conn,
    source_schema: Option<&str>,
    target: &Conn,
) -> Result<(), TenancyError> {
    let mut dump_cmd = source.command("pg_dump");
    dump_cmd
        .arg("--no-owner")
        .arg("--no-acl")
        .arg("--format=plain")
        .arg("--no-publications")
        .arg("--no-subscriptions");
    if let Some(s) = source_schema {
        dump_cmd.arg(format!("--schema={s}"));
    }
    dump_cmd.stdout(Stdio::piped());
    dump_cmd.stderr(Stdio::piped());
    let mut dump = spawn(&mut dump_cmd, "pg_dump")?;
    let dump_stdout = dump.stdout.take().expect("pg_dump stdout was piped");

    let mut restore_cmd = target.psql();
    restore_cmd.arg("--single-transaction");
    restore_cmd.stdin(dump_stdout);
    let restored = run(restore_cmd, "psql restore");
    let dump_status = dump.wait().map_err(TenancyError::Io)?;
    // A failed restore closes the pipe, which fails the dump too.
    restored?;
    if !dump_status.success() {
        return Err(TenancyError::Validation(format!(
            "pg_dump failed (exit {:?}): {}",
            dump_status.code(),
            stderr_of(&mut dump)
        )));
    }
    Ok(())
}

fn spawn(cmd: &mut std::process::Command, what: &str) -> Result<std::process::Child, TenancyError> {
    cmd.spawn().map_err(|e| {
        TenancyError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("failed to spawn `{what}`: {e} — install Postgres client tools?"),
        ))
    })
}

/// Run `cmd` to the end; a non-zero exit is an error with its stderr.
fn run(mut cmd: std::process::Command, what: &str) -> Result<(), TenancyError> {
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let child = spawn(&mut cmd, what)?;
    // Our copy of a piped stdin would keep its writer from seeing EPIPE.
    drop(cmd);
    let out = child.wait_with_output().map_err(TenancyError::Io)?;
    if out.status.success() {
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "{what} failed (exit {:?}): {}",
        out.status.code(),
        first_lines(&String::from_utf8_lossy(&out.stderr))
    )))
}

fn stderr_of(child: &mut std::process::Child) -> String {
    use std::io::Read;
    let mut buf = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut buf);
    }
    first_lines(&buf)
}

fn first_lines(s: &str) -> String {
    s.lines().take(6).collect::<Vec<_>>().join(" | ")
}

/// Check `rustango_users` is reachable at the new location; rows or not.
/// Schema-qualified: through the search_path, `public.rustango_users`
/// passed for an empty target schema (#1864).
async fn smoke_check(target_url: &str, target_schema: Option<&str>) -> Result<(), TenancyError> {
    use crate::sql::sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(target_url)
        .await?;
    let stmt = format!(
        "SELECT 1 FROM {}.rustango_users LIMIT 1",
        quote_ident(target_schema.unwrap_or("public"))
    );
    let res = crate::sql::sqlx::query(&stmt).fetch_optional(&pool).await;
    pool.close().await;
    res?;
    Ok(())
}

use crate::sql::connect_diagnosis::redact as redact_url;

#[cfg(test)]
mod tests {
    use super::*;

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn parse_args_requires_slug() {
        let err = parse_args(&[]).unwrap_err();
        assert!(format!("{err}").contains("missing <slug>"));
    }

    #[test]
    fn parse_args_requires_to_flag() {
        let err = parse_args(&s(&["acme"])).unwrap_err();
        assert!(format!("{err}").contains("--to"));
    }

    #[test]
    fn parse_args_rejects_unknown_to_value() {
        let err = parse_args(&s(&["acme", "--to", "redis"])).unwrap_err();
        assert!(format!("{err}").contains("--to must be"));
    }

    #[test]
    fn parse_args_accepts_schema_target() {
        let parsed = parse_args(&s(&["acme", "--to", "schema"])).unwrap();
        assert_eq!(parsed.slug, "acme");
        assert_eq!(parsed.target, StorageMode::Schema);
        assert!(!parsed.dry_run);
    }

    #[test]
    fn parse_args_accepts_database_target_with_url() {
        let parsed = parse_args(&s(&[
            "acme",
            "--to",
            "database",
            "--database-url",
            "postgres://x:y@h/d",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(parsed.target, StorageMode::Database);
        assert_eq!(parsed.database_url.as_deref(), Some("postgres://x:y@h/d"));
        assert!(parsed.dry_run);
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_args(&s(&["acme", "--to", "schema", "--foo"])).unwrap_err();
        assert!(format!("{err}").contains("unknown argument `--foo`"));
    }

    #[test]
    fn redact_url_masks_password() {
        assert_eq!(
            redact_url("postgres://alice:secret@db.example.com/mydb"),
            "postgres://alice:***@db.example.com/mydb",
        );
    }

    #[test]
    fn redact_url_handles_no_password() {
        assert_eq!(
            redact_url("postgres://alice@db.example.com/mydb"),
            "postgres://alice@db.example.com/mydb",
        );
    }

    #[test]
    fn redact_url_handles_no_scheme() {
        assert_eq!(redact_url("just-a-string"), "just-a-string");
    }

    /// #1864 — the password goes to `PGPASSWORD`, never argv.
    #[test]
    fn conn_keeps_the_password_out_of_the_url() {
        let c = Conn::new("postgres://al:p%40ss@w/rd@h:5432/app?sslmode=require");
        assert_eq!(c.url, "postgres://al@h:5432/app?sslmode=require");
        assert_eq!(c.password.as_deref(), Some("p@ss@w/rd"));
        let c = Conn::new("postgres://al@h/app?password=s%3Dx&sslmode=disable");
        assert_eq!(c.url, "postgres://al@h/app?sslmode=disable");
        assert_eq!(c.password.as_deref(), Some("s=x"));
        let c = Conn::new("postgres://h/app").database("stage");
        assert_eq!((c.url.as_str(), c.password), ("postgres://h/stage", None));
        let c = Conn::new("postgres://al:pw@h/app?sslmode=require").database("stage");
        assert_eq!(c.url, "postgres://al@h/stage?sslmode=require");
        let argv: Vec<_> = c.command("psql").get_args().map(|a| a.to_owned()).collect();
        assert!(!argv.iter().any(|a| a.to_string_lossy().contains("pw")));
    }

    /// #1864 — an empty target schema fails even with `public.rustango_users`
    /// present. Own database, so `public` is ours; skips without `DATABASE_URL`.
    #[tokio::test]
    async fn smoke_check_looks_only_in_the_target_schema() {
        use crate::sql::sqlx;
        let Ok(admin_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let admin = sqlx::PgPool::connect(&admin_url).await.unwrap();
        let drop_db = "DROP DATABASE IF EXISTS rustango_t1864 WITH (FORCE)";
        sqlx::query(drop_db).execute(&admin).await.unwrap();
        sqlx::query("CREATE DATABASE rustango_t1864")
            .execute(&admin)
            .await
            .unwrap();
        let (base, _) = admin_url.rsplit_once('/').unwrap();
        let url = format!("{base}/rustango_t1864");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        for stmt in [
            "CREATE TABLE public.rustango_users (id BIGINT)",
            "CREATE SCHEMA acme",
            "CREATE SCHEMA moved",
            "CREATE TABLE moved.rustango_users (id BIGINT)",
        ] {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }

        assert!(smoke_check(&url, Some("acme")).await.is_err());
        smoke_check(&url, Some("moved")).await.unwrap();
        smoke_check(&url, None).await.unwrap();
        sqlx::query("DROP TABLE public.rustango_users")
            .execute(&pool)
            .await
            .unwrap();
        assert!(smoke_check(&url, None).await.is_err());

        pool.close().await;
        sqlx::query(drop_db).execute(&admin).await.unwrap();
    }
}
