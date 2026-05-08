//! `migrate-tenant-storage <slug> --to schema|database` — flip a
//! populated tenant between `schema` and `database` storage modes
//! safely. Closes the gap that locks `storage_mode` in the operator
//! console (`op_orgs_edit.html` LOCKED_ORG_FIELDS) — without this
//! verb, an operator who flips the field via raw SQL ends up with
//! the data sitting at the OLD location and no resolver pointing at
//! it (split-brain).
//!
//! ## Algorithm
//!
//! 1. Look up `Org` by slug. Validate target ≠ current storage_mode.
//! 2. Provision the target storage:
//!    - schema → ensure `CREATE SCHEMA IF NOT EXISTS <name>` on the
//!      registry DB.
//!    - database → caller passes `--database-url`. Database must
//!      already exist (we don't `CREATE DATABASE` — that's a
//!      single-statement decision the operator should own).
//! 3. `pg_dump` the source (schema-scoped or full DB), pipe into
//!    `psql` against the target.
//! 4. Single-statement transaction: `UPDATE rustango_orgs SET
//!    storage_mode = ?, database_url = ?, schema_name = ? WHERE
//!    slug = ?`.
//! 5. `TenantPools::invalidate(slug)` so the next request rebuilds
//!    the cached pool against the new storage.
//! 6. Smoke check: `SELECT 1 FROM rustango_users LIMIT 1` against the
//!    new location. Failure here triggers a best-effort revert.
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
//! * It does NOT create the target database. Schema-mode targets
//!   land via `CREATE SCHEMA IF NOT EXISTS`; database-mode targets
//!   need `createdb` / `CREATE DATABASE` to have run already.
//! * It does NOT verify the data row counts match between source
//!   and target — only that the new location is reachable. Strict
//!   parity is the operator's responsibility.

use std::io::Write;
use std::process::Stdio;

use crate::core::Column as _;
use crate::sql::Fetcher;
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
        .fetch(pools.registry())
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

    // 3. Provision the target.
    if let Some(target) = &target_schema {
        let stmt = format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(target));
        crate::sql::sqlx::query(&stmt)
            .execute(pools.registry())
            .await?;
        writeln!(writer, "  ensured target schema `{target}`")?;
    }

    // 4. Dump → restore. Streams pg_dump stdout into psql stdin so
    // we never buffer the full snapshot in memory.
    writeln!(writer, "  starting pg_dump → psql pipe…")?;
    pg_dump_to_psql(
        &source_url,
        source_schema.as_deref(),
        &target_url,
        target_schema.as_deref(),
    )?;
    writeln!(writer, "  data move OK")?;

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
    let prior_storage_mode = org.storage_mode.clone();
    let prior_database_url = org.database_url.clone();
    let prior_schema_name = org.schema_name.clone();
    org.storage_mode = new_storage_mode;
    org.database_url = new_database_url.clone();
    org.schema_name = new_schema_name.clone();
    org.save(pools.registry()).await?;
    writeln!(writer, "  Org row updated")?;

    // 6. Evict cached pool, smoke-check the new location.
    pools.invalidate(&parsed.slug).await;
    writeln!(writer, "  cached tenant pool evicted")?;

    if let Err(e) = smoke_check(&parsed.target, &target_url, target_schema.as_deref()).await {
        writeln!(
            writer,
            "  smoke-check FAILED: {e} — reverting Org row to {prior_storage_mode}"
        )?;
        org.storage_mode = prior_storage_mode;
        org.database_url = prior_database_url;
        org.schema_name = prior_schema_name;
        let _ = org.save(pools.registry()).await;
        pools.invalidate(&parsed.slug).await;
        return Err(TenancyError::Validation(format!(
            "smoke-check failed against new storage; Org row reverted: {e}"
        )));
    }

    writeln!(writer, "  smoke-check OK")?;
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

/// Pipe `pg_dump <source>` into `psql <target>`. When the source is
/// schema-scoped, pass `--schema=<name>` to pg_dump. When the target
/// is schema-scoped, prepend a `SET search_path TO <name>, public;`
/// statement to the restore stream so unqualified objects land in
/// the right schema.
fn pg_dump_to_psql(
    source_url: &str,
    source_schema: Option<&str>,
    target_url: &str,
    target_schema: Option<&str>,
) -> Result<(), TenancyError> {
    let mut dump_cmd = std::process::Command::new("pg_dump");
    dump_cmd
        .arg("--no-owner")
        .arg("--no-acl")
        .arg("--format=plain")
        .arg("--no-publications")
        .arg("--no-subscriptions");
    if let Some(s) = source_schema {
        dump_cmd.arg(format!("--schema={s}"));
    }
    dump_cmd.arg(source_url);
    dump_cmd.stdout(Stdio::piped());
    dump_cmd.stderr(Stdio::piped());
    let mut dump = dump_cmd.spawn().map_err(|e| {
        TenancyError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("failed to spawn `pg_dump`: {e} — install Postgres client tools?"),
        ))
    })?;
    let dump_stdout = dump.stdout.take().expect("pg_dump stdout was piped");

    let mut restore_cmd = std::process::Command::new("psql");
    restore_cmd
        .arg("--quiet")
        .arg("--no-psqlrc")
        .arg("-v")
        .arg("ON_ERROR_STOP=1");
    if let Some(s) = target_schema {
        // Prepend a search_path SET so the dump's CREATE TABLE / etc.
        // land in the target schema. pg_dump writes tables with their
        // SOURCE schema qualified — when we set --schema on the
        // source side that's the source schema name. To rename into a
        // different target schema, we'd need sed-style rewrite; for
        // the same-name case (slug → slug, the default), no rewrite
        // is needed and the schema_name is identical on both sides.
        restore_cmd.arg("-c");
        restore_cmd.arg(format!("SET search_path TO {}, public", quote_ident(s)));
    }
    restore_cmd.arg(target_url);
    restore_cmd.stdin(dump_stdout);
    restore_cmd.stderr(Stdio::piped());
    let mut restore = restore_cmd.spawn().map_err(|e| {
        TenancyError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("failed to spawn `psql`: {e} — install Postgres client tools?"),
        ))
    })?;

    let dump_status = dump.wait().map_err(TenancyError::Io)?;
    let restore_status = restore.wait().map_err(TenancyError::Io)?;

    if !dump_status.success() {
        let stderr = dump
            .stderr
            .take()
            .map(|mut s| {
                use std::io::Read;
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf);
                buf
            })
            .unwrap_or_default();
        return Err(TenancyError::Validation(format!(
            "pg_dump failed (exit {:?}): {}",
            dump_status.code(),
            stderr.lines().take(6).collect::<Vec<_>>().join(" | ")
        )));
    }
    if !restore_status.success() {
        let stderr = restore
            .stderr
            .take()
            .map(|mut s| {
                use std::io::Read;
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf);
                buf
            })
            .unwrap_or_default();
        return Err(TenancyError::Validation(format!(
            "psql restore failed (exit {:?}): {}",
            restore_status.code(),
            stderr.lines().take(6).collect::<Vec<_>>().join(" | ")
        )));
    }
    Ok(())
}

/// Open a one-shot pool against the new location, run `SELECT 1
/// FROM rustango_users LIMIT 1`. The query passes whether or not
/// any rows exist — what we're checking is that the table is
/// reachable, not that it has data.
async fn smoke_check(
    target: &StorageMode,
    target_url: &str,
    target_schema: Option<&str>,
) -> Result<(), TenancyError> {
    use crate::sql::sqlx::postgres::PgPoolOptions;
    let opts = PgPoolOptions::new().max_connections(1);
    let pool = if let (StorageMode::Schema, Some(schema)) = (target, target_schema) {
        let schema_owned: std::sync::Arc<str> = std::sync::Arc::from(schema);
        opts.after_connect(move |conn, _meta| {
            let schema = std::sync::Arc::clone(&schema_owned);
            Box::pin(async move {
                let stmt = format!("SET search_path TO {}, public", quote_ident(&schema));
                crate::sql::sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect(target_url)
        .await?
    } else {
        opts.connect(target_url).await?
    };
    let _row: Option<(i32,)> = crate::sql::sqlx::query_as("SELECT 1 FROM rustango_users LIMIT 1")
        .fetch_optional(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

/// Replace the password segment of a Postgres conninfo string with
/// `***` so log lines / writer output don't leak credentials. Best-
/// effort: if the URL doesn't follow the `postgres://user:pass@host`
/// shape we just return it verbatim.
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after = &url[scheme_end + 3..];
        if let Some(at) = after.find('@') {
            let creds = &after[..at];
            if let Some(colon) = creds.find(':') {
                let user = &creds[..colon];
                let rest = &after[at..];
                return format!("{}://{user}:***{rest}", &url[..scheme_end]);
            }
        }
    }
    url.to_owned()
}

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
}
