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

use crate::diff::render_changes;
use crate::file::{self, Migration, Operation};
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
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let ddl = render_changes(std::slice::from_ref(change), &mig.snapshot)
                    .map_err(MigrateError::Validation)?;
                for stmt in ddl {
                    sqlx::query(&stmt).execute(&mut *tx).await?;
                }
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(&mut *tx).await?;
            }
        }
    }
    sqlx::query("INSERT INTO __rustango_migrations__ (name) VALUES ($1)")
        .bind(&mig.name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn apply_loose(pool: &PgPool, mig: &Migration) -> Result<(), MigrateError> {
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let ddl = render_changes(std::slice::from_ref(change), &mig.snapshot)
                    .map_err(MigrateError::Validation)?;
                for stmt in ddl {
                    sqlx::query(&stmt).execute(pool).await?;
                }
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(pool).await?;
            }
        }
    }
    sqlx::query("INSERT INTO __rustango_migrations__ (name) VALUES ($1)")
        .bind(&mig.name)
        .execute(pool)
        .await?;
    Ok(())
}
