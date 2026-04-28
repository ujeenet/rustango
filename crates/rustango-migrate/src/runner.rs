//! Apply DDL against a live Postgres pool.
//!
//! Two flows live here:
//!
//! * [`apply_all`] / [`drop_all`] walk the inventory registry directly
//!   — useful for fresh-DB bootstrap and tear-down in tests. No file
//!   I/O, no ledger.
//! * [`migrate`] applies pending migration files from a directory,
//!   using the `__rustango_migrations__` ledger table to skip files
//!   that have already been applied. Each file runs in its own
//!   transaction by default (Django-style — partial progress across
//!   files is recoverable).

use std::collections::HashSet;
use std::path::Path;

use rustango_core::{inventory, ModelEntry, ModelSchema};
use rustango_sql::sqlx::{self, PgPool, Row};

use crate::diff::render_changes_split;
use crate::file::{self, Migration, Operation};
use crate::invert::invert;
use crate::snapshot::SchemaSnapshot;
use crate::{ddl, MigrateError};

/// Name of the bookkeeping table — stores one row per applied
/// migration. Double-underscored to avoid colliding with user tables.
pub const LEDGER_TABLE: &str = "__rustango_migrations__";

const CREATE_LEDGER_SQL: &str = "CREATE TABLE IF NOT EXISTS __rustango_migrations__ (\
    name TEXT PRIMARY KEY, \
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW())";

/// Collect every registered model's schema into a `Vec`. Order is the
/// order of registration (linker order); callers that care should sort.
#[must_use]
pub fn registered_models() -> Vec<&'static ModelSchema> {
    inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema)
        .collect()
}

/// Run `CREATE TABLE` for every registered model, then every model's FK
/// `ALTER TABLE` constraints. Two-phase so create order doesn't matter.
///
/// # Errors
/// Returns [`MigrateError`] for any sqlx failure (connection, syntax,
/// constraint violation).
pub async fn apply_all(pool: &PgPool) -> Result<(), MigrateError> {
    let models = registered_models();

    for model in &models {
        let sql = ddl::create_table_sql(model);
        sqlx::query(&sql).execute(pool).await?;
    }
    for model in &models {
        for sql in ddl::create_constraints_sql(model) {
            sqlx::query(&sql).execute(pool).await?;
        }
    }
    Ok(())
}

/// `DROP TABLE IF EXISTS … CASCADE` for every registered model. CASCADE
/// makes order irrelevant — FKs go away with the parent table.
///
/// # Errors
/// Returns [`MigrateError`] for any sqlx failure.
pub async fn drop_all(pool: &PgPool) -> Result<(), MigrateError> {
    for model in registered_models() {
        let sql = ddl::drop_table_sql(model, /* if_exists */ true, /* cascade */ true);
        sqlx::query(&sql).execute(pool).await?;
    }
    Ok(())
}

/// Ensure the ledger table exists, then apply every pending migration
/// in `dir` (lex-sorted by name) to `pool`. Already-applied migrations
/// are skipped.
///
/// Each migration runs in its own transaction unless its `atomic`
/// field is `false` (e.g. for `CREATE INDEX CONCURRENTLY`). On
/// failure within an atomic migration the file's changes roll back
/// cleanly; **prior** files stay applied (their commits already
/// happened), so re-running `migrate` after fixing the offender will
/// pick up where it left off.
///
/// Returns the migrations that were newly applied (could be empty).
///
/// # Errors
/// Returns [`MigrateError::Io`]/[`MigrateError::Json`]/[`MigrateError::Validation`]
/// for file problems, [`MigrateError::Driver`] for SQL failures.
pub async fn migrate(pool: &PgPool, dir: &Path) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger(pool).await?;

    let all = file::list_dir(dir)?;
    let applied = applied_set(pool).await?;
    let pending: Vec<Migration> = all
        .into_iter()
        .filter(|m| !applied.contains(&m.name))
        .collect();

    let mut newly = Vec::with_capacity(pending.len());
    for mig in pending {
        if mig.atomic {
            apply_atomic(pool, &mig).await?;
        } else {
            apply_loose(pool, &mig).await?;
        }
        newly.push(mig);
    }
    Ok(newly)
}

/// Set of migration names already recorded in the ledger.
///
/// # Errors
/// Returns [`MigrateError::Driver`] for any sqlx failure (including a
/// missing ledger table — call [`ensure_ledger`] first).
pub async fn applied_set(pool: &PgPool) -> Result<HashSet<String>, MigrateError> {
    let rows = sqlx::query("SELECT name FROM __rustango_migrations__")
        .fetch_all(pool)
        .await?;
    let mut out = HashSet::with_capacity(rows.len());
    for row in rows {
        out.insert(row.try_get::<String, _>("name")?);
    }
    Ok(out)
}

/// Bootstrap the ledger table if it doesn't exist. Idempotent and
/// safe to run from concurrent processes — Postgres' `CREATE TABLE
/// IF NOT EXISTS` is *not* race-free against concurrent creators
/// (they can both pass the existence check and then collide on the
/// catalog), so the bootstrap is serialized via a transaction-scoped
/// advisory lock.
///
/// # Errors
/// Returns [`MigrateError::Driver`] for any sqlx failure.
pub async fn ensure_ledger(pool: &PgPool) -> Result<(), MigrateError> {
    // Stable arbitrary key — must be the same every call. "RUST" in ASCII hex.
    const LOCK_KEY: i64 = 0x5255_5354;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    sqlx::query(CREATE_LEDGER_SQL).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn apply_atomic(pool: &PgPool, mig: &Migration) -> Result<(), MigrateError> {
    let mut tx = pool.begin().await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let batch = render_changes_split(std::slice::from_ref(change), &mig.snapshot)
                    .map_err(MigrateError::Validation)?;
                for stmt in batch.immediate {
                    sqlx::query(&stmt).execute(&mut *tx).await?;
                }
                deferred_fks.extend(batch.deferred_fks);
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(&mut *tx).await?;
            }
        }
    }
    for stmt in deferred_fks {
        sqlx::query(&stmt).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO __rustango_migrations__ (name) VALUES ($1)")
        .bind(&mig.name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Move the database to a specific migration target — forward or back.
///
/// Compares `target` to the current head (lex-greatest applied
/// migration name in `dir`) and walks the right direction:
///
/// * `target > head` → apply pending migrations whose name lies in
///   `(head, target]`, in lex order.
/// * `target == head` → no-op.
/// * `target < head` → unapply migrations whose name lies in
///   `(target, head]`, in **reverse** lex order.
/// * `target == "zero"` → unapply every applied migration. Special-
///   cased so users have a stable way to wipe the schema's migration
///   state without having to think about which file is "earliest".
///
/// Returns the migrations that were applied or unapplied (the caller
/// can compare against [`applied_set`] before/after to infer
/// direction). Returns an empty `Vec` if the target was already the
/// current head.
///
/// # Errors
/// * [`MigrateError::Validation`] if `target` doesn't match any file
///   in `dir` (and isn't `"zero"`).
/// * Any error [`migrate`] or [`unapply`] would raise.
pub async fn migrate_to(
    pool: &PgPool,
    dir: &Path,
    target: &str,
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = applied_set(pool).await?;

    if target == "zero" {
        return unapply_all_in_order(pool, dir, &all, &applied).await;
    }

    if !all.iter().any(|m| m.name == target) {
        return Err(MigrateError::Validation(format!(
            "target migration `{target}` not found in {}",
            dir.display()
        )));
    }

    let head = all
        .iter()
        .rev()
        .find(|m| applied.contains(&m.name))
        .map(|m| m.name.clone());

    let mut touched = Vec::new();
    match head {
        None => {
            // Nothing applied — forward up to and including target.
            for mig in all.into_iter().filter(|m| m.name.as_str() <= target) {
                apply_one(pool, &mig).await?;
                touched.push(mig);
            }
        }
        Some(h) => {
            use std::cmp::Ordering;
            match target.cmp(h.as_str()) {
                Ordering::Equal => {}
                Ordering::Greater => {
                    for mig in all.into_iter().filter(|m| {
                        m.name.as_str() > h.as_str()
                            && m.name.as_str() <= target
                            && !applied.contains(&m.name)
                    }) {
                        apply_one(pool, &mig).await?;
                        touched.push(mig);
                    }
                }
                Ordering::Less => {
                    let mut to_unapply: Vec<Migration> = all
                        .into_iter()
                        .filter(|m| {
                            m.name.as_str() > target
                                && m.name.as_str() <= h.as_str()
                                && applied.contains(&m.name)
                        })
                        .collect();
                    to_unapply.reverse();
                    for mig in to_unapply {
                        unapply(pool, dir, &mig.name).await?;
                        touched.push(mig);
                    }
                }
            }
        }
    }
    Ok(touched)
}

/// Apply pending migrations from an in-memory `&[(name, json)]` slice.
///
/// Built for deployments where shipping a `migrations/` folder
/// alongside the binary is awkward (Docker images, single-binary
/// distribution). Pair with the [`embed_migrations!`] proc-macro from
/// `rustango-macros` (re-exported as `rustango::embed_migrations!`),
/// which scans a directory at compile time and emits the slice via
/// `include_str!` per file. The macro emits content in lex-sorted
/// order, but this function re-sorts defensively.
///
/// Each entry's first item must equal the migration's `name` field
/// — a divergence would mean the slice was hand-built incorrectly.
///
/// [`embed_migrations!`]: https://docs.rs/rustango/0.1/rustango/macro.embed_migrations.html
///
/// # Errors
/// As [`migrate`], plus [`MigrateError::Validation`] when an entry
/// key doesn't match the migration's own name.
pub async fn migrate_embedded(
    pool: &PgPool,
    embedded: &[(&str, &str)],
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger(pool).await?;

    let mut all: Vec<Migration> = Vec::with_capacity(embedded.len());
    for (name, json) in embedded {
        let mig = file::parse(json)?;
        if mig.name != *name {
            return Err(MigrateError::Validation(format!(
                "embedded entry key `{name}` doesn't match migration `name` field `{}`",
                mig.name,
            )));
        }
        all.push(mig);
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));

    let applied = applied_set(pool).await?;
    let pending: Vec<Migration> = all
        .into_iter()
        .filter(|m| !applied.contains(&m.name))
        .collect();

    let mut newly = Vec::with_capacity(pending.len());
    for mig in pending {
        if mig.atomic {
            apply_atomic(pool, &mig).await?;
        } else {
            apply_loose(pool, &mig).await?;
        }
        newly.push(mig);
    }
    Ok(newly)
}

/// Step back `steps` applied migrations (Alembic's `downgrade -N`).
///
/// `downgrade(pool, dir, 1)` rolls back the most recently applied
/// migration. `downgrade(pool, dir, n)` rolls back the `n` most
/// recent. If `n` exceeds the number of applied migrations, every
/// applied migration is rolled back. `n == 0` is a no-op.
///
/// # Errors
/// As [`unapply`] for each step.
pub async fn downgrade(
    pool: &PgPool,
    dir: &Path,
    steps: usize,
) -> Result<Vec<Migration>, MigrateError> {
    if steps == 0 {
        return Ok(Vec::new());
    }
    ensure_ledger(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = applied_set(pool).await?;

    let applied_in_order: Vec<Migration> = all
        .into_iter()
        .filter(|m| applied.contains(&m.name))
        .collect();
    if applied_in_order.is_empty() {
        return Ok(Vec::new());
    }

    let n = steps.min(applied_in_order.len());
    let to_unapply: Vec<Migration> = applied_in_order.into_iter().rev().take(n).collect();

    let mut touched = Vec::with_capacity(to_unapply.len());
    for mig in to_unapply {
        unapply(pool, dir, &mig.name).await?;
        touched.push(mig);
    }
    Ok(touched)
}

async fn apply_one(pool: &PgPool, mig: &Migration) -> Result<(), MigrateError> {
    if mig.atomic {
        apply_atomic(pool, mig).await
    } else {
        apply_loose(pool, mig).await
    }
}

async fn unapply_all_in_order(
    pool: &PgPool,
    dir: &Path,
    all: &[Migration],
    applied: &HashSet<String>,
) -> Result<Vec<Migration>, MigrateError> {
    let mut to_unapply: Vec<Migration> = all
        .iter()
        .filter(|m| applied.contains(&m.name))
        .cloned()
        .collect();
    to_unapply.reverse();
    let mut touched = Vec::with_capacity(to_unapply.len());
    for mig in to_unapply {
        unapply(pool, dir, &mig.name).await?;
        touched.push(mig);
    }
    Ok(touched)
}

/// Roll back a single applied migration.
///
/// Loads `dir/{name}.json`, looks up its predecessor (or empty for
/// the first migration) for snapshot context, computes the inverse
/// op list via [`crate::invert::invert`], and executes it in a
/// transaction (or loose if the original `atomic: false`). Removes
/// the entry from `__rustango_migrations__` on success.
///
/// **What "roll back" means here:** schema reversal restores shape,
/// not data — `DropColumn` then `unapply` does NOT bring back the
/// column's row values. Data reversal is only as good as the
/// `reverse_sql` you wrote in the migration file; if you wrote
/// `reverse_sql: "DELETE FROM x"`, that's what runs.
///
/// **Irreversible migrations** (`reversible: false` on any data op)
/// fail fast before any DB write, with an error that names the op.
///
/// # Errors
/// * [`MigrateError::Validation`] — irreversible op, missing
///   migration file, missing predecessor.
/// * [`MigrateError::Driver`] — SQL failure during rollback.
pub async fn unapply(pool: &PgPool, dir: &Path, name: &str) -> Result<Migration, MigrateError> {
    ensure_ledger(pool).await?;

    let all = file::list_dir(dir)?;
    let target = all
        .iter()
        .find(|m| m.name == name)
        .cloned()
        .ok_or_else(|| {
            MigrateError::Validation(format!("migration `{name}` not found in {}", dir.display()))
        })?;

    let prev_snapshot = match &target.prev {
        None => SchemaSnapshot { tables: vec![] },
        Some(prev_name) => all
            .iter()
            .find(|m| &m.name == prev_name)
            .map(|m| m.snapshot.clone())
            .ok_or_else(|| {
                MigrateError::Validation(format!(
                    "migration `{name}` declares prev=`{prev_name}` but that file is missing in {}",
                    dir.display()
                ))
            })?,
    };

    let inverted = invert(&target.forward, &prev_snapshot)?;

    if target.atomic {
        unapply_atomic(pool, &target, &inverted, &prev_snapshot).await?;
    } else {
        unapply_loose(pool, &target, &inverted, &prev_snapshot).await?;
    }

    Ok(target)
}

async fn unapply_atomic(
    pool: &PgPool,
    target: &Migration,
    inverted: &[Operation],
    snapshot: &SchemaSnapshot,
) -> Result<(), MigrateError> {
    let mut tx = pool.begin().await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in inverted {
        match op {
            Operation::Schema(change) => {
                let batch = render_changes_split(std::slice::from_ref(change), snapshot)
                    .map_err(MigrateError::Validation)?;
                for stmt in batch.immediate {
                    sqlx::query(&stmt).execute(&mut *tx).await?;
                }
                deferred_fks.extend(batch.deferred_fks);
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(&mut *tx).await?;
            }
        }
    }
    for stmt in deferred_fks {
        sqlx::query(&stmt).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM __rustango_migrations__ WHERE name = $1")
        .bind(&target.name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn unapply_loose(
    pool: &PgPool,
    target: &Migration,
    inverted: &[Operation],
    snapshot: &SchemaSnapshot,
) -> Result<(), MigrateError> {
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in inverted {
        match op {
            Operation::Schema(change) => {
                let batch = render_changes_split(std::slice::from_ref(change), snapshot)
                    .map_err(MigrateError::Validation)?;
                for stmt in batch.immediate {
                    sqlx::query(&stmt).execute(pool).await?;
                }
                deferred_fks.extend(batch.deferred_fks);
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(pool).await?;
            }
        }
    }
    for stmt in deferred_fks {
        sqlx::query(&stmt).execute(pool).await?;
    }
    sqlx::query("DELETE FROM __rustango_migrations__ WHERE name = $1")
        .bind(&target.name)
        .execute(pool)
        .await?;
    Ok(())
}

async fn apply_loose(pool: &PgPool, mig: &Migration) -> Result<(), MigrateError> {
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let batch = render_changes_split(std::slice::from_ref(change), &mig.snapshot)
                    .map_err(MigrateError::Validation)?;
                for stmt in batch.immediate {
                    sqlx::query(&stmt).execute(pool).await?;
                }
                deferred_fks.extend(batch.deferred_fks);
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(pool).await?;
            }
        }
    }
    for stmt in deferred_fks {
        sqlx::query(&stmt).execute(pool).await?;
    }
    sqlx::query("INSERT INTO __rustango_migrations__ (name) VALUES ($1)")
        .bind(&mig.name)
        .execute(pool)
        .await?;
    Ok(())
}
