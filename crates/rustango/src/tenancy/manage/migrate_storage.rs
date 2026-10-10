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
//!      registry user needs PG 15+ and `CREATEDB` (or a superuser on
//!      13/14) for a staging database. The registry gets the extensions
//!      the tenant uses and it lacks, in `public`: trusted ones, or those
//!      named by `--allow-extension`. An interrupted run can
//!      leave a `rustango_stage_*` database to drop by hand.
//!    - database → caller passes `--database-url`. Database must
//!      already exist (we don't `CREATE DATABASE` — that's a
//!      single-statement decision the operator should own). Its empty
//!      `public` is replaced by the restored schema, renamed to `public`;
//!      both emptiness and the right to drop it are checked first.
//! 3. Deactivate the tenant and wait `--drain-secs` (default: the tenant
//!    cache TTL) so servers stop writing the old copy. Requests already
//!    running and workers that bypass the resolver are not stopped.
//!    `active` comes back on success, failure and Ctrl-C, by an update
//!    guarded by `active = false`, so a suspension made during the move is
//!    still undone. If the process dies, `edit-tenant <slug> --activate`.
//!
//!    `pg_dump` the source (schema-scoped or full DB), pipe into
//!    `psql` against the target. Into a schema it goes through a
//!    staging database that renames `public` first.
//! 4. Smoke check: `SELECT 1 FROM <schema>.rustango_users LIMIT 1`
//!    against the new location, before the Org row moves.
//! 5. `UPDATE rustango_orgs SET storage_mode, database_url, schema_name
//!    (, active)` — only those columns, one short guarded update.
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
//!   final message names the old schema or database; drop it by hand
//!   once the new side is healthy. Not with `purge-tenant`: that drops
//!   the tenant's *new* storage and its Org row (#2382).
//! * It does NOT create the target database. Database-mode targets
//!   need `createdb` / `CREATE DATABASE` to have run already.
//! * It does NOT verify the data row counts match between source
//!   and target — only that the new location is reachable. Strict
//!   parity is the operator's responsibility.

use std::io::Write;
use std::process::Stdio;

use crate::core::Column as _;
use crate::sql::UpdaterPool as _;
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
    /// Untrusted extensions the move may create (`--allow-extension`).
    allow_extensions: Vec<String>,
    /// How long the tenant stays inactive before the dump (#2383).
    drain_secs: u64,
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
    let org = orgs
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

    // Secret references (`env://…`) resolve to connect; only the
    // reference is printed and stored (#2384).
    let (source_ref, source_url) = match current {
        StorageMode::Schema => (registry_url.to_owned(), registry_url.to_owned()),
        StorageMode::Database => (
            org.database_url.clone().unwrap_or_default(),
            pools.resolved_database_url(&org).await?,
        ),
    };
    let source_schema = match current {
        StorageMode::Schema => Some(SchemaName::parse(org.effective_schema())?),
        StorageMode::Database => None,
    };
    let target_schema = match parsed.target {
        StorageMode::Schema => Some(SchemaName::parse(crate::tenancy::org::effective_schema(
            parsed.schema_name.as_deref(),
            &parsed.slug,
        ))?),
        StorageMode::Database => None,
    };
    if let Some(schema) = &target_schema {
        let registry = pools.registry_pool();
        if crate::tenancy::org_host::schema_claimed(&registry, &schema.0, org.id.get().copied())
            .await?
        {
            return Err(TenancyError::Validation(format!(
                "schema `{schema}` is already used by another tenant"
            )));
        }
    }
    let (target_ref, target_url) = match &parsed.database_url {
        Some(reference) if parsed.target == StorageMode::Database => {
            (reference.clone(), pools.resolve_secret(reference).await?)
        }
        _ => (registry_url.to_owned(), registry_url.to_owned()),
    };

    writeln!(
        writer,
        "migrate-tenant-storage `{}`: {} → {}",
        parsed.slug, current, parsed.target,
    )?;
    if let Some(s) = &source_schema {
        writeln!(writer, "  source: {} (schema `{s}`)", shown(&source_ref))?;
    } else {
        writeln!(writer, "  source: {}", shown(&source_ref))?;
    }
    if let Some(s) = &target_schema {
        writeln!(writer, "  target: {} (schema `{s}`)", shown(&target_ref))?;
    } else {
        writeln!(writer, "  target: {}", shown(&target_ref))?;
    }

    if parsed.dry_run {
        writeln!(writer, "  [dry-run] no changes — exit.")?;
        return Ok(());
    }

    // 2. Every refusal before the tenant goes offline.
    let restore = match (&target_schema, &source_schema) {
        (Some(schema), _) => {
            let allowed = &parsed.allow_extensions;
            Restore::IntoSchema(
                plan_into_schema(pools.registry(), &source_url, schema, allowed).await?,
            )
        }
        // A database-mode tenant lives in `public` (#2189).
        (None, Some(schema)) => {
            check_public_is_replaceable(&target_url).await?;
            // The dump names their objects where the registry has them; they
            // move with the tables into `public` (#2210).
            let used = extensions_used_by(pools.registry(), &schema.0).await?;
            refuse_fixed(used.iter().filter(|e| !e.relocatable))?;
            let pool = crate::sql::sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&target_url)
                .await?;
            // Only what the target lacks needs trust to create (#2385).
            let refused = async {
                let missing = missing_on(&pool, &used, &schema.0).await?;
                refuse_untrusted(&pool, &missing, &parsed.allow_extensions, "the target").await
            }
            .await;
            pool.close().await;
            refused?;
            // One in the tenant's schema comes with the dump, which creates
            // that schema; `before` cannot (#2386).
            let (own, other): (Vec<&Extension>, _) =
                used.iter().partition(|e| e.schema == schema.0);
            let extensions = own.iter().map(|e| e.name.clone()).collect();
            let before = other.iter().flat_map(|e| e.create_in(&e.schema)).collect();
            let mut after: Vec<String> = used.iter().flat_map(|e| e.move_to(&schema.0)).collect();
            // `--no-acl` drops the default grant, which other app roles need.
            after.extend([
                "DROP SCHEMA public".to_owned(),
                format!("ALTER SCHEMA {} RENAME TO public", quote_ident(&schema.0)),
                "GRANT USAGE ON SCHEMA public TO PUBLIC".to_owned(),
            ]);
            Restore::Dump {
                extensions,
                before,
                after,
            }
        }
        (None, None) => Restore::Dump {
            extensions: Vec::new(),
            before: Vec::new(),
            after: Vec::new(),
        },
    };

    // 3. Offline for the move: a write to the old copy after the dump
    // would be lost (#2383).
    let id = org
        .id
        .get()
        .copied()
        .ok_or_else(|| TenancyError::Validation("Org row has no PK".into()))?;
    let slug = &parsed.slug;
    // Before going offline, so Ctrl-C reaches the code that comes back.
    let mut interrupt = Interrupt::listen()?;
    if org.active {
        writeln!(
            writer,
            "  deactivating the tenant; if this run dies, run `edit-tenant {slug} --activate`"
        )?;
        writeln!(
            writer,
            "  waiting {} s for running servers' tenant caches…",
            parsed.drain_secs
        )?;
        set_active(pools, slug, id, false).await?;
    } else {
        writeln!(
            writer,
            "  the tenant is inactive and stays so; if an earlier move left it so, \
             run `edit-tenant {slug} --activate` afterwards"
        )?;
    }
    MoveCtx {
        pools,
        parsed: &parsed,
        id,
        reactivate: org.active,
        source_url: &source_url,
        source_schema: source_schema.as_ref(),
        target_url: &target_url,
        target_schema: target_schema.as_ref(),
    }
    .run(restore, &mut interrupt, writer)
    .await?;

    writeln!(writer, "  Org row updated")?;
    writeln!(
        writer,
        "  running servers switch to the new location within {} s (their tenant cache TTL)",
        crate::tenancy::resolver::CACHE_TTL.as_secs()
    )?;
    writeln!(
        writer,
        "  ✓ migrated `{}` to {} mode.",
        parsed.slug, parsed.target,
    )?;
    writeln!(
        writer,
        "  {}",
        old_copy_advice(source_schema.as_ref(), &source_ref)
    )?;
    Ok(())
}

/// How the data moves, decided before the tenant goes offline.
enum Restore {
    /// database → schema, through a staging database.
    IntoSchema(SchemaPlan),
    /// One `pg_dump | psql`, with statements around the dump.
    Dump {
        /// Extensions the dump itself creates (`pg_dump --extension`).
        extensions: Vec<String>,
        before: Vec<String>,
        after: Vec<String>,
    },
}

/// SIGINT / SIGTERM, caught while the tenant is offline so the move can
/// bring it back (#2383).
struct Interrupt {
    #[cfg(unix)]
    signals: [tokio::signal::unix::Signal; 2],
}

impl Interrupt {
    fn listen() -> Result<Self, TenancyError> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                signals: [
                    signal(SignalKind::interrupt()).map_err(TenancyError::Io)?,
                    signal(SignalKind::terminate()).map_err(TenancyError::Io)?,
                ],
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let [int, term] = &mut self.signals;
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// The part of the move that runs while the tenant is offline.
struct MoveCtx<'a> {
    pools: &'a TenantPools,
    parsed: &'a MigrateStorageArgs,
    id: i64,
    /// The move deactivated the tenant, so it brings it back.
    reactivate: bool,
    source_url: &'a str,
    source_schema: Option<&'a SchemaName>,
    target_url: &'a str,
    target_schema: Option<&'a SchemaName>,
}

impl MoveCtx<'_> {
    /// No lock is held across the move: the switch and the reactivation
    /// are short updates guarded by `active = false`. A suspension made
    /// during the move is still undone by them; a "moved by migrate"
    /// marker column would fix that (0.61.0).
    async fn run<W: Write + Send>(
        self,
        restore: Restore,
        interrupt: &mut Interrupt,
        writer: &mut W,
    ) -> Result<(), TenancyError> {
        let done = tokio::select! {
            r = self.copy(restore, writer) => r,
            () = interrupt.recv() => Err(TenancyError::Validation("interrupted".into())),
        };
        let done = match done {
            Ok(()) => self.switch().await,
            Err(e) => Err(e),
        };
        let result = match done {
            Ok(()) => Ok(()),
            Err(e) if self.reactivate => match self.come_back().await {
                Ok(()) => Err(e),
                Err(again) => Err(self.left_inactive(format!("{e}; {again}"))),
            },
            Err(e) => Err(e),
        };
        crate::tenancy::invalidate_org_cache();
        self.pools.invalidate(&self.parsed.slug).await;
        result
    }

    /// The move's own `active = false`, guarded the same way as the switch.
    fn offline(&self) -> crate::query::QuerySet<Org> {
        let rows = Org::objects().where_(Org::id.eq(self.id));
        if self.reactivate {
            rows.where_(Org::active.eq(false))
        } else {
            rows
        }
    }

    async fn come_back(&self) -> Result<(), TenancyError> {
        let n = self
            .offline()
            .update()
            .set_typed(Org::active.set(true))
            .execute_pool(&self.pools.registry_pool())
            .await?;
        expect_one(n, &self.parsed.slug)
    }

    /// `e`, plus the way back when the move left the tenant inactive.
    fn left_inactive(&self, e: impl std::fmt::Display) -> TenancyError {
        let slug = &self.parsed.slug;
        if self.reactivate {
            TenancyError::Validation(format!(
                "{e}; the tenant stays inactive: run `edit-tenant {slug} --activate`"
            ))
        } else {
            TenancyError::Validation(e.to_string())
        }
    }

    /// Drain, then 3. dump → restore and 4. smoke check.
    async fn copy<W: Write + Send>(
        &self,
        restore: Restore,
        writer: &mut W,
    ) -> Result<(), TenancyError> {
        if self.reactivate {
            tokio::time::sleep(std::time::Duration::from_secs(self.parsed.drain_secs)).await;
        }
        // Streams pg_dump stdout into psql stdin so we never buffer the
        // full snapshot in memory.
        writeln!(writer, "  starting pg_dump → psql pipe…")?;
        let (source, target) = (Conn::new(self.source_url), Conn::new(self.target_url));
        match restore {
            Restore::IntoSchema(plan) => {
                run_into_schema(self.pools.registry(), &source, &target, plan).await?;
            }
            Restore::Dump {
                extensions,
                before,
                after,
            } => {
                let scope = self.source_schema;
                pg_dump_to_psql(&source, scope, &extensions, &target, &before, &after)?;
            }
        }
        writeln!(writer, "  data move OK")?;

        // 4. Before the Org row moves, so a bad restore leaves it in place.
        let target_name = self.target_schema.map(|s| s.0.as_str());
        if let Err(e) = smoke_check(self.target_url, target_name).await {
            // The restore created it, so a rerun would hit "schema exists".
            let Some(s) = target_name else {
                return Err(TenancyError::Validation(format!(
                    "smoke-check failed: {e}; recreate the target database before a rerun"
                )));
            };
            let drop = format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(s));
            crate::sql::sqlx::query(&drop)
                .execute(self.pools.registry())
                .await?;
            return Err(TenancyError::Validation(format!(
                "smoke-check failed: {e}; the restored schema was dropped"
            )));
        }
        writeln!(writer, "  smoke-check OK")?;
        Ok(())
    }

    /// 5. Only the storage columns (and `active`), one guarded update.
    async fn switch(&self) -> Result<(), TenancyError> {
        let (database_url, schema_name) = match self.parsed.target {
            StorageMode::Database => (self.parsed.database_url.clone(), None),
            StorageMode::Schema => (None, self.target_schema.map(|s| s.0.clone())),
        };
        let mut update = self
            .offline()
            .update()
            .set_typed(Org::storage_mode.set(self.parsed.target.as_str().to_owned()))
            .set_typed(Org::database_url.set(database_url))
            .set_typed(Org::schema_name.set(schema_name));
        if self.reactivate {
            update = update.set_typed(Org::active.set(true));
        }
        let n = update.execute_pool(&self.pools.registry_pool()).await?;
        expect_one(n, &self.parsed.slug)
    }
}

/// Set the tenant's `active` column alone, and drop this process's caches.
async fn set_active(
    pools: &TenantPools,
    slug: &str,
    id: i64,
    active: bool,
) -> Result<(), TenancyError> {
    let n = Org::objects()
        .where_(Org::id.eq(id))
        .update()
        .set_typed(Org::active.set(active))
        .execute_pool(&pools.registry_pool())
        .await?;
    crate::tenancy::invalidate_org_cache();
    pools.invalidate(slug).await;
    expect_one(n, slug)
}

/// An Org row update must hit exactly the tenant's row: none means it was
/// deleted, or reactivated during the move.
fn expect_one(updated: u64, slug: &str) -> Result<(), TenancyError> {
    if updated == 1 {
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "tenant `{slug}` was deleted or reactivated meanwhile; its Org row was not updated"
    )))
}

/// A URL or keyword conninfo, its password masked.
fn shown(conninfo: &str) -> String {
    if conninfo.contains("://") {
        return redact_url(conninfo);
    }
    conninfo
        .split(' ')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if k == "password" => "password=***".to_owned(),
            _ => kv.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where the old copy is and how to drop it. Never `purge-tenant`: it
/// drops the tenant's current (new) storage and its Org row (#2382).
/// `purge-tenant`'s check that no other tenant shares it is not done here.
fn old_copy_advice(source_schema: Option<&SchemaName>, source_ref: &str) -> String {
    let what = match source_schema {
        Some(s) => format!(
            "schema `{s}` on the registry database ({}); once the new location is \
             healthy and no other tenant uses it, run `DROP SCHEMA {} CASCADE` there",
            shown(source_ref),
            quote_ident(&s.0)
        ),
        None => format!(
            "the database at {}; once the new location is healthy and no other tenant \
             uses it, drop that database",
            shown(source_ref)
        ),
    };
    format!(
        "The old copy is still in {what} by hand. Not with `purge-tenant`: it would drop \
         the new location and the Org row."
    )
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
    let mut allow_extensions = Vec::new();
    let mut drain_secs = crate::tenancy::resolver::CACHE_TTL.as_secs();
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
            "--allow-extension" => {
                allow_extensions.push(next_value(&mut iter, "--allow-extension")?);
            }
            "--drain-secs" => {
                let v = next_value(&mut iter, "--drain-secs")?;
                drain_secs = v.parse().map_err(|_| {
                    TenancyError::Validation(format!("--drain-secs must be a number, got `{v}`"))
                })?;
            }
            "--dry-run" => dry_run = true,
            "--help" | "-h" => {
                return Err(TenancyError::Validation(
                    "migrate-tenant-storage <slug> --to schema|database \
                     [--database-url <conninfo>] [--schema-name <s>] \
                     [--allow-extension <name>]... [--drain-secs <n>] [--dry-run]"
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
        allow_extensions,
        drain_secs,
        dry_run,
    })
}

use crate::dbshell::LibpqConn as Conn;

impl Conn {
    fn psql(&self) -> std::process::Command {
        let mut cmd = self.command("psql");
        cmd.args(["--quiet", "--no-psqlrc", "-v", "ON_ERROR_STOP=1"]);
        cmd
    }
}

/// A target schema name that provisioning accepts (`[a-z0-9_-]`), so
/// pg_dump's `--schema` pattern can neither fold its case nor match it
/// as a wildcard.
#[derive(Clone)]
struct SchemaName(String);

impl SchemaName {
    fn parse(name: &str) -> Result<Self, TenancyError> {
        crate::tenancy::provision::validate_schema_name(name).map_err(TenancyError::Validation)?;
        Ok(Self(name.to_owned()))
    }
}

impl std::fmt::Display for SchemaName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An extension as one database has it.
struct Extension {
    name: String,
    schema: String,
    relocatable: bool,
}

impl Extension {
    fn create_in(&self, schema: &str) -> [String; 2] {
        [
            format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema)),
            format!(
                "CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {} CASCADE",
                quote_ident(&self.name),
                quote_ident(schema)
            ),
        ]
    }

    fn move_to(&self, schema: &str) -> [String; 2] {
        [
            format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema)),
            format!(
                "ALTER EXTENSION {} SET SCHEMA {}",
                quote_ident(&self.name),
                quote_ident(schema)
            ),
        ]
    }
}

const EXTENSIONS: &str = "SELECT DISTINCT e.extname, n.nspname, e.extrelocatable \
     FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace";

async fn fetch_extensions(
    pool: &crate::sql::sqlx::PgPool,
    sql: &str,
    bind: &str,
) -> Result<Vec<Extension>, TenancyError> {
    let rows: Vec<(String, String, bool)> = crate::sql::sqlx::query_as(sql)
        .bind(bind)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(name, schema, relocatable)| Extension {
            name,
            schema,
            relocatable,
        })
        .collect())
}

/// The extensions that the objects of schema `$1` use. `own` is the
/// schema's tables, types and functions that no extension owns, and what
/// hangs off them (columns, defaults, CHECKs, indexes, view rules,
/// triggers); `refs` is what those reference, with array and domain types
/// mapped to their element and base types.
const USED_BY: &str = "WITH RECURSIVE own(classid, objid) AS ( \
        SELECT o.classid, o.objid FROM ( \
            SELECT 'pg_class'::regclass AS classid, c.oid AS objid FROM pg_class c \
             WHERE c.relnamespace = $1::text::regnamespace AND NOT EXISTS ( \
                SELECT 1 FROM pg_depend x WHERE x.classid = 'pg_type'::regclass \
                   AND x.objid = c.reltype AND x.deptype = 'e') \
            UNION ALL SELECT 'pg_type'::regclass, t.oid FROM pg_type t \
             WHERE t.typnamespace = $1::text::regnamespace AND t.typrelid = 0 \
               AND t.typcategory <> 'A' \
            UNION ALL SELECT 'pg_proc'::regclass, p.oid FROM pg_proc p \
             WHERE p.pronamespace = $1::text::regnamespace \
        ) o WHERE NOT EXISTS (SELECT 1 FROM pg_depend x \
            WHERE x.classid = o.classid AND x.objid = o.objid AND x.deptype = 'e') \
      UNION \
        SELECT d.classid, d.objid FROM pg_depend d \
          JOIN own ON d.refclassid = own.classid AND d.refobjid = own.objid \
         WHERE d.deptype IN ('n', 'a', 'i') \
    ), refs(classid, objid) AS ( \
        SELECT d.refclassid, d.refobjid FROM pg_depend d \
          JOIN own ON d.classid = own.classid AND d.objid = own.objid \
      UNION \
        SELECT 'pg_type'::regclass, v.base FROM refs \
          JOIN pg_type t ON refs.classid = 'pg_type'::regclass AND t.oid = refs.objid, \
          LATERAL (VALUES (t.typelem), (t.typbasetype)) v(base) \
         WHERE v.base <> 0 \
    ) \
    SELECT DISTINCT e.extname, n.nspname, e.extrelocatable FROM refs \
      JOIN pg_depend m ON m.classid = refs.classid AND m.objid = refs.objid \
       AND m.deptype = 'e' \
      JOIN pg_extension e ON e.oid = m.refobjid \
      JOIN pg_namespace n ON n.oid = e.extnamespace \
     WHERE e.extname <> 'plpgsql' ORDER BY 1";

/// The extensions `schema`'s objects use, and the ones those require.
async fn extensions_used_by(
    pool: &crate::sql::sqlx::PgPool,
    schema: &str,
) -> Result<Vec<Extension>, TenancyError> {
    let mut used = fetch_extensions(pool, USED_BY, schema).await?;
    let one = format!("{EXTENSIONS} WHERE e.extname = $1");
    let mut i = 0;
    while i < used.len() {
        let requires: Option<Option<Vec<String>>> = crate::sql::sqlx::query_scalar(
            "SELECT v.requires::text[] FROM pg_available_extension_versions v \
             JOIN pg_extension e ON e.extname = v.name AND e.extversion = v.version \
             WHERE v.name = $1",
        )
        .bind(&used[i].name)
        .fetch_optional(pool)
        .await?;
        for name in requires.flatten().unwrap_or_default() {
            if !used.iter().any(|e| e.name == name) {
                used.extend(fetch_extensions(pool, &one, &name).await?);
            }
        }
        i += 1;
    }
    Ok(used)
}

/// The extensions of `used` that `pool`'s database lacks. One it has must
/// sit where the dump names it, or the restore fails after the drain
/// (#2385); one in the tenant's schema the dump creates itself (#2386).
async fn missing_on<'e>(
    pool: &crate::sql::sqlx::PgPool,
    used: &'e [Extension],
    tenant_schema: &str,
) -> Result<Vec<&'e Extension>, TenancyError> {
    let sql = format!("{EXTENSIONS} WHERE e.extname = $1");
    let mut missing = Vec::new();
    for e in used {
        let Some(found) = fetch_extensions(pool, &sql, &e.name).await?.pop() else {
            missing.push(e);
            continue;
        };
        if e.schema == tenant_schema {
            return Err(TenancyError::Validation(format!(
                "extension `{}` is already on the target (schema `{}`), but the dump \
                 creates it with the tenant's schema: drop it there",
                e.name, found.schema
            )));
        }
        if found.schema != e.schema {
            return Err(TenancyError::Validation(format!(
                "extension `{}` is in schema `{}` on the target, but the tenant uses it \
                 from `{}`: move it there",
                e.name, found.schema, e.schema
            )));
        }
    }
    Ok(missing)
}

/// Refuse to create on `pool`'s server an extension it does not trust,
/// unless the operator allowed it: one untrusted extension in a tenant
/// must not install itself in a shared database.
async fn refuse_untrusted(
    pool: &crate::sql::sqlx::PgPool,
    creating: &[&Extension],
    allowed: &[String],
    place: &str,
) -> Result<(), TenancyError> {
    let mut refused = Vec::new();
    for e in creating.iter().filter(|e| !allowed.contains(&e.name)) {
        let trusted: Option<bool> = crate::sql::sqlx::query_scalar(
            "SELECT v.trusted FROM pg_available_extension_versions v \
             JOIN pg_available_extensions a ON a.name = v.name \
              AND a.default_version = v.version WHERE v.name = $1",
        )
        .bind(&e.name)
        .fetch_optional(pool)
        .await?;
        if trusted != Some(true) {
            refused.push(e.name.as_str());
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "extension(s) {} would be created on {place}, which does not trust them: \
         create them there by hand, or pass --allow-extension <name>",
        refused.join(", ")
    )))
}

/// Refuse the extensions that would have to change schema but cannot.
fn refuse_fixed<'e>(fixed: impl Iterator<Item = &'e Extension>) -> Result<(), TenancyError> {
    let names: Vec<&str> = fixed.map(|e| e.name.as_str()).collect();
    if names.is_empty() {
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "extension(s) {} cannot change schema (not relocatable), so this tenant cannot move",
        names.join(", ")
    )))
}

/// Restore a database-mode tenant's `public` into `schema` on the
/// registry. pg_dump names every object `public.`, so `public` is renamed
/// in a staging database first (#1864).
struct SchemaPlan {
    schema: SchemaName,
    /// Extensions the registry lacks, created before the restore.
    create: Vec<String>,
    /// Run in staging: the rename, and the extensions' moves.
    moves: Vec<String>,
}

/// The extensions it uses go back where the registry has them, or to
/// `public`, and the registry gets any it lacks (#2210).
async fn plan_into_schema(
    registry: &crate::sql::sqlx::PgPool,
    source_url: &str,
    schema: &SchemaName,
    allowed: &[String],
) -> Result<SchemaPlan, TenancyError> {
    use crate::sql::sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(source_url)
        .await?;
    let extensions = extensions_used_by(&pool, "public").await;
    pool.close().await;
    let extensions = extensions?;
    let mut create = Vec::new();
    let mut creating = Vec::new();
    let mut moves = vec![format!(
        "ALTER SCHEMA public RENAME TO {}",
        quote_ident(&schema.0)
    )];
    let mut fixed = Vec::new();
    let sql = format!("{EXTENSIONS} WHERE e.extname = $1");
    for e in &extensions {
        let installed = fetch_extensions(registry, &sql, &e.name).await?;
        let home = installed.into_iter().next().map_or_else(
            || {
                create.extend(e.create_in("public"));
                creating.push(e);
                "public".to_owned()
            },
            |r| r.schema,
        );
        // Where the rename leaves it in staging.
        let at = if e.schema == "public" {
            &schema.0
        } else {
            &e.schema
        };
        if *at != home {
            moves.extend(e.move_to(&home));
            if !e.relocatable {
                fixed.push(e);
            }
        }
    }
    refuse_fixed(fixed.into_iter())?;
    refuse_untrusted(registry, &creating, allowed, "the registry").await?;
    Ok(SchemaPlan {
        schema: schema.clone(),
        create,
        moves,
    })
}

async fn run_into_schema(
    registry: &crate::sql::sqlx::PgPool,
    source: &Conn,
    target: &Conn,
    plan: SchemaPlan,
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
    let moved = pg_dump_to_psql(source, None, &[], &stage, &[], &plan.moves)
        .and_then(|()| pg_dump_to_psql(&stage, Some(&plan.schema), &[], target, &plan.create, &[]));
    let dropped = crate::sql::sqlx::query(&format!("DROP DATABASE {quoted} WITH (FORCE)"))
        .execute(registry)
        .await;
    moved?;
    dropped?;
    Ok(())
}

/// Pipe `pg_dump <source>` into `psql <target>` in one transaction. When
/// the source is schema-scoped, pass `--schema=<name>` to pg_dump, and
/// `--extension` (pg_dump 14+) for each of `extensions`.
fn pg_dump_to_psql(
    source: &Conn,
    source_schema: Option<&SchemaName>,
    extensions: &[String],
    target: &Conn,
    before: &[String],
    after: &[String],
) -> Result<(), TenancyError> {
    let mut dump_cmd = source.command("pg_dump");
    dump_cmd
        .arg("--no-owner")
        .arg("--no-acl")
        .arg("--format=plain")
        .arg("--no-publications")
        .arg("--no-subscriptions");
    if let Some(s) = source_schema {
        dump_cmd.arg(format!("--schema={}", s.0));
    }
    // Quoted, so the pattern is the literal name.
    for e in extensions {
        dump_cmd.arg(format!("--extension={}", quote_ident(e)));
    }
    dump_cmd.stdout(Stdio::piped());
    dump_cmd.stderr(Stdio::piped());
    let mut dump = spawn(&mut dump_cmd, "pg_dump")?;
    let dump_stdout = dump.stdout.take().expect("pg_dump stdout was piped");

    let mut restore_cmd = target.psql();
    // psql reads no stdin once `-c` is given, so `-f -` (#1864); all in
    // one transaction, in this order.
    restore_cmd.arg("--single-transaction");
    for stmt in before {
        restore_cmd.arg("-c").arg(stmt);
    }
    restore_cmd.args(["-f", "-"]);
    for stmt in after {
        restore_cmd.arg("-c").arg(stmt);
    }
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

/// The target's `public` must be empty and ours to drop, or the restore
/// fails at `DROP SCHEMA public` (#2189).
async fn check_public_is_replaceable(target_url: &str) -> Result<(), TenancyError> {
    use crate::sql::sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(target_url)
        .await?;
    let owned: Option<bool> = crate::sql::sqlx::query_scalar(
        "SELECT pg_has_role(nspowner, 'USAGE') FROM pg_namespace WHERE nspname = 'public'",
    )
    .fetch_optional(&pool)
    .await?;
    // What DROP SCHEMA would refuse on, without an extension's members.
    let objects: Vec<String> = crate::sql::sqlx::query_scalar(
        "SELECT pg_describe_object(d.classid, d.objid, d.objsubid) FROM pg_depend d \
         WHERE d.refclassid = 'pg_namespace'::regclass \
           AND d.refobjid = 'public'::regnamespace AND d.deptype = 'n' \
           AND NOT EXISTS (SELECT 1 FROM pg_depend e WHERE e.classid = d.classid \
                           AND e.objid = d.objid AND e.deptype = 'e') \
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    if owned != Some(true) {
        return Err(TenancyError::Validation(
            "the target database's `public` schema must be droppable by this user: \
             connect as the database owner (PG 15+) or a superuser"
                .into(),
        ));
    }
    if !objects.is_empty() {
        let shown = objects
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        return Err(TenancyError::Validation(format!(
            "the target database's `public` schema must be empty, it holds {} object(s): \
             {shown}. Use an empty database; keep extensions in their own schema",
            objects.len()
        )));
    }
    Ok(())
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
    fn parse_args_collects_allowed_extensions() {
        let parsed = parse_args(&s(&[
            "acme",
            "--to",
            "schema",
            "--allow-extension",
            "postgis",
            "--allow-extension",
            "dblink",
        ]))
        .unwrap();
        assert_eq!(parsed.allow_extensions, ["postgis", "dblink"]);
    }

    /// #2383 — waits out the tenant cache TTL unless told otherwise.
    #[test]
    fn parse_args_drain_secs() {
        let parsed = parse_args(&s(&["acme", "--to", "schema"])).unwrap();
        let ttl = crate::tenancy::resolver::CACHE_TTL.as_secs();
        assert_eq!(parsed.drain_secs, ttl);
        let parsed = parse_args(&s(&["acme", "--to", "schema", "--drain-secs", "5"])).unwrap();
        assert_eq!(parsed.drain_secs, 5);
        assert!(parse_args(&s(&["acme", "--to", "schema", "--drain-secs", "x"])).is_err());
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

    /// #2384 — a printed conninfo never shows its password.
    #[test]
    fn shown_masks_keyword_and_url_passwords() {
        assert_eq!(
            shown("host=db user=al password=s3cret dbname=app"),
            "host=db user=al password=*** dbname=app"
        );
        assert_eq!(
            shown("postgres://al:s3cret@h/app"),
            "postgres://al:***@h/app"
        );
        assert_eq!(shown("env://ACME_DB"), "env://ACME_DB");
    }

    /// An Org update that hits no row is an error, not a success.
    #[test]
    fn expect_one_refuses_a_missing_row() {
        assert!(expect_one(1, "acme").is_ok());
        let err = expect_one(0, "acme").unwrap_err().to_string();
        assert!(err.contains("`acme` was deleted or reactivated"), "{err}");
    }

    /// pg_dump reads `--schema` as a pattern; only literal names pass.
    #[test]
    fn schema_name_is_a_literal() {
        assert!(SchemaName::parse("acme_2-x").is_ok());
        for bad in ["", "Acme", "a*", "a.b", "a?", "a\"b", &"a".repeat(64)] {
            assert!(SchemaName::parse(bad).is_err(), "{bad}");
        }
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
