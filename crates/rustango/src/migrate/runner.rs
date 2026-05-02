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

use crate::core::{inventory, ModelEntry, ModelSchema};
use crate::sql::sqlx::{self, PgPool, Row};

use super::diff::render_changes_split;
use super::file::{self, Migration, Operation};
use super::invert::invert;
use super::snapshot::SchemaSnapshot;
use super::{ddl, MigrateError};

/// Default bookkeeping-table name — stores one row per applied
/// migration. Double-underscored to avoid colliding with user
/// tables. Override per-app via `Builder::ledger`.
pub const LEDGER_TABLE: &str = "__rustango_migrations__";

/// Per-app migration runner config. Lets two rustango apps live in
/// the same Postgres database without colliding on the default
/// `__rustango_migrations__` ledger table — each app picks its own
/// ledger name and the runners stay independent.
///
/// All verbs are mirrored on the `Builder` so a custom-ledger app
/// has the same surface as the default free functions:
///
/// ```ignore
/// let mine = Builder::default().ledger("__myapp_migrations__");
/// mine.migrate(&pool, dir).await?;
/// mine.applied_set(&pool).await?;
/// ```
///
/// Ledger names must be valid SQL identifiers (`[A-Za-z_][A-Za-z0-9_]*`,
/// ≤ 63 bytes). `.ledger("…")` panics if not — this is a programming
/// error caught at config time, not a runtime input.
#[derive(Debug, Clone, Copy)]
pub struct Builder {
    ledger: &'static str,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            ledger: LEDGER_TABLE,
        }
    }
}

impl Builder {
    /// Equivalent to `Builder::default()`. Provided for symmetry with
    /// the rest of the workspace's builder constructors.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the ledger table name. Must be a valid SQL
    /// identifier (`[A-Za-z_][A-Za-z0-9_]*`, ≤ 63 bytes).
    ///
    /// # Panics
    /// Panics if `name` isn't a valid SQL identifier — the ledger
    /// name is interpolated into DDL (`CREATE TABLE`, `INSERT INTO`,
    /// `SELECT FROM`) so we refuse anything that could escape
    /// quoting.
    #[must_use]
    pub fn ledger(mut self, name: &'static str) -> Self {
        validate_ledger_name(name);
        self.ledger = name;
        self
    }

    /// The configured ledger table name.
    #[must_use]
    pub fn ledger_name(&self) -> &'static str {
        self.ledger
    }

    /// As [`migrate`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`migrate`].
    pub async fn migrate(
        &self,
        pool: &PgPool,
        dir: &Path,
    ) -> Result<Vec<Migration>, MigrateError> {
        migrate_with_ledger(pool, dir, self.ledger).await
    }

    /// As [`migrate_to`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`migrate_to`].
    pub async fn migrate_to(
        &self,
        pool: &PgPool,
        dir: &Path,
        target: &str,
    ) -> Result<Vec<Migration>, MigrateError> {
        migrate_to_with_ledger(pool, dir, target, self.ledger).await
    }

    /// As [`migrate_embedded`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`migrate_embedded`].
    pub async fn migrate_embedded(
        &self,
        pool: &PgPool,
        embedded: &[(&str, &str)],
    ) -> Result<Vec<Migration>, MigrateError> {
        migrate_embedded_with_ledger(pool, embedded, self.ledger).await
    }

    /// As [`migrate_dry_run`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`migrate_dry_run`].
    pub async fn migrate_dry_run(
        &self,
        pool: &PgPool,
        dir: &Path,
    ) -> Result<Vec<MigrationPreview>, MigrateError> {
        migrate_dry_run_with_ledger(pool, dir, self.ledger).await
    }

    /// As [`downgrade`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`downgrade`].
    pub async fn downgrade(
        &self,
        pool: &PgPool,
        dir: &Path,
        steps: usize,
    ) -> Result<Vec<Migration>, MigrateError> {
        downgrade_with_ledger(pool, dir, steps, self.ledger).await
    }

    /// As [`unapply`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`unapply`].
    pub async fn unapply(
        &self,
        pool: &PgPool,
        dir: &Path,
        name: &str,
    ) -> Result<Migration, MigrateError> {
        unapply_with_ledger(pool, dir, name, self.ledger).await
    }

    /// As [`unapply_force`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`unapply_force`].
    pub async fn unapply_force(
        &self,
        pool: &PgPool,
        dir: &Path,
        name: &str,
    ) -> Result<Migration, MigrateError> {
        unapply_force_with_ledger(pool, dir, name, self.ledger).await
    }

    /// As [`applied_set`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`applied_set`].
    pub async fn applied_set(&self, pool: &PgPool) -> Result<HashSet<String>, MigrateError> {
        applied_set_for(pool, self.ledger).await
    }

    /// As [`ensure_ledger`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`ensure_ledger`].
    pub async fn ensure_ledger(&self, pool: &PgPool) -> Result<(), MigrateError> {
        ensure_ledger_for(pool, self.ledger).await
    }
}

fn validate_ledger_name(name: &str) {
    // SQL identifier syntax: leading letter or `_`, then letters /
    // digits / `_`. Postgres limits identifiers to 63 bytes.
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
    assert!(
        valid,
        "Builder::ledger({name:?}) is not a valid SQL identifier — \
         must match [A-Za-z_][A-Za-z0-9_]* and be ≤ 63 bytes"
    );
}

/// Postgres advisory-lock key used to serialize concurrent
/// `migrate` / `migrate_to` / `unapply` / `downgrade` /
/// `migrate_embedded` calls across processes. Without this lock,
/// peer boots both query `applied_set`, both see the same pending
/// list, both try to apply it, and one loses the race with a
/// `relation already exists` or PK violation on the ledger INSERT.
///
/// "RUSTMIGT" in ASCII hex.
const MIGRATE_LOCK_KEY: i64 = 0x5255_5354_4d49_4754;

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
    Builder::default().migrate(pool, dir).await
}

async fn migrate_with_ledger(
    pool: &PgPool,
    dir: &Path,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger_for(pool, ledger).await?;
    with_migrate_lock(pool, async {
        let all = file::list_dir(dir)?;
        let applied = applied_set_for(pool, ledger).await?;
        let pending: Vec<Migration> = all
            .into_iter()
            .filter(|m| !applied.contains(&m.name))
            .collect();

        let mut newly = Vec::with_capacity(pending.len());
        for mig in pending {
            apply_one(pool, &mig, ledger).await?;
            newly.push(mig);
        }
        Ok(newly)
    })
    .await
}

/// Hold the migrate advisory lock for the duration of `body`, then
/// release it (best-effort) before returning. Peers calling any
/// migrate-shaped operation block until the holder releases.
///
/// The lock is **session-scoped**, so we acquire a dedicated
/// connection from the pool, hold it for the whole body, and
/// explicitly unlock before dropping it back to the pool. (Dropping
/// alone wouldn't release, since pooled connections survive between
/// uses.)
async fn with_migrate_lock<F, R>(pool: &PgPool, body: F) -> Result<R, MigrateError>
where
    F: std::future::Future<Output = Result<R, MigrateError>>,
{
    use crate::sql::{Dialect as _, Postgres};
    // Dialect dispatch: Postgres returns the `pg_advisory_lock` SQL;
    // SQLite (when added in slice 10.5) returns `None` and the body
    // runs without a session lock (SQLite's single-writer model
    // achieves the same exclusion via `BEGIN EXCLUSIVE`).
    let dialect = Postgres;
    let mut lock_conn = pool.acquire().await?;
    if let Some(acquire_sql) = dialect.acquire_session_lock_sql() {
        sqlx::query(&acquire_sql)
            .bind(MIGRATE_LOCK_KEY)
            .execute(&mut *lock_conn)
            .await?;
    }
    let result = body.await;
    // Always try to release. If unlock fails (e.g. connection died),
    // Postgres releases on session close so we won't deadlock peers
    // forever — and we want the original error from `result` to
    // propagate, not a noisy unlock error.
    if let Some(release_sql) = dialect.release_session_lock_sql() {
        let _ = sqlx::query(&release_sql)
            .bind(MIGRATE_LOCK_KEY)
            .execute(&mut *lock_conn)
            .await;
    }
    result
}

/// Set of migration names already recorded in the default ledger
/// table (`__rustango_migrations__`). For a custom ledger, build a
/// [`Builder`] with `.ledger("…")` and call its `applied_set` method.
///
/// # Errors
/// Returns [`MigrateError::Driver`] for any sqlx failure (including a
/// missing ledger table — call [`ensure_ledger`] first).
pub async fn applied_set(pool: &PgPool) -> Result<HashSet<String>, MigrateError> {
    applied_set_for(pool, LEDGER_TABLE).await
}

async fn applied_set_for(
    pool: &PgPool,
    ledger: &str,
) -> Result<HashSet<String>, MigrateError> {
    let rows = sqlx::query(&format!("SELECT name FROM {ledger}"))
        .fetch_all(pool)
        .await?;
    let mut out = HashSet::with_capacity(rows.len());
    for row in rows {
        out.insert(row.try_get::<String, _>("name")?);
    }
    Ok(out)
}

/// Bootstrap the default ledger table (`__rustango_migrations__`)
/// if it doesn't exist. Idempotent and safe to run from concurrent
/// processes — Postgres' `CREATE TABLE IF NOT EXISTS` is *not*
/// race-free against concurrent creators (they can both pass the
/// existence check and then collide on the catalog), so the
/// bootstrap is serialized via a transaction-scoped advisory lock.
///
/// For a custom ledger, build a [`Builder`] with `.ledger("…")` and
/// call its `ensure_ledger` method.
///
/// # Errors
/// Returns [`MigrateError::Driver`] for any sqlx failure.
pub async fn ensure_ledger(pool: &PgPool) -> Result<(), MigrateError> {
    ensure_ledger_for(pool, LEDGER_TABLE).await
}

async fn ensure_ledger_for(pool: &PgPool, ledger: &str) -> Result<(), MigrateError> {
    use crate::sql::{Dialect as _, Postgres};
    // Stable arbitrary key — must be the same every call. "RUST" in ASCII hex.
    const LOCK_KEY: i64 = 0x5255_5354;
    let dialect = Postgres;
    let mut tx = pool.begin().await?;
    // Postgres returns `pg_advisory_xact_lock(...)`. SQLite returns
    // `None` (its `BEGIN` already gates concurrent CREATE TABLE).
    if let Some(xact_lock_sql) = dialect.acquire_xact_lock_sql() {
        sqlx::query(&xact_lock_sql)
            .bind(LOCK_KEY)
            .execute(&mut *tx)
            .await?;
    }
    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {ledger} (\
         name TEXT PRIMARY KEY, \
         applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW())"
    );
    sqlx::query(&create_sql).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// One pending migration the dry-run would apply.
///
/// `statements` is the literal SQL the runner would execute, in
/// order: each `SchemaChange` op's immediate DDL, each `DataOp`'s
/// `sql`, then any deferred FK ALTERs, then the
/// `INSERT INTO __rustango_migrations__` ledger row. Atomic
/// migrations also get synthetic `BEGIN`/`COMMIT` markers so the
/// reader can see where the transaction boundary is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPreview {
    pub name: String,
    /// `true` if the migration would run inside a transaction (the
    /// `atomic` flag on the file).
    pub atomic: bool,
    pub statements: Vec<String>,
}

/// Compute the SQL `migrate(pool, dir)` would execute, without
/// running any of it. Reads the ledger to know what's pending; never
/// writes. Output is one [`MigrationPreview`] per pending migration,
/// in apply order.
///
/// Used by `manage migrate --dry-run`.
///
/// # Errors
/// As [`migrate`] minus the SQL execution — file I/O, JSON parse,
/// chain validation, plus the `applied_set` read.
pub async fn migrate_dry_run(
    pool: &PgPool,
    dir: &Path,
) -> Result<Vec<MigrationPreview>, MigrateError> {
    Builder::default().migrate_dry_run(pool, dir).await
}

async fn migrate_dry_run_with_ledger(
    pool: &PgPool,
    dir: &Path,
    ledger: &str,
) -> Result<Vec<MigrationPreview>, MigrateError> {
    ensure_ledger_for(pool, ledger).await?;
    let all = file::list_dir(dir)?;
    let applied = applied_set_for(pool, ledger).await?;
    let pending: Vec<Migration> = all
        .into_iter()
        .filter(|m| !applied.contains(&m.name))
        .collect();

    let mut out = Vec::with_capacity(pending.len());
    for mig in pending {
        out.push(preview_migration(&mig, ledger)?);
    }
    Ok(out)
}

/// Build a [`MigrationPreview`] for a single migration. Pure —
/// no DB access. Same render path as `apply_atomic` / `apply_loose`
/// but the statements stream into a `Vec<String>` instead of a tx.
fn preview_migration(mig: &Migration, ledger: &str) -> Result<MigrationPreview, MigrateError> {
    let mut statements = Vec::new();
    let mut deferred_fks: Vec<String> = Vec::new();
    if mig.atomic {
        statements.push("BEGIN".to_string());
    }
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let batch = render_changes_split(std::slice::from_ref(change), &mig.snapshot)
                    .map_err(MigrateError::Validation)?;
                statements.extend(batch.immediate);
                deferred_fks.extend(batch.deferred_fks);
            }
            Operation::Data(d) => {
                statements.push(d.sql.clone());
            }
        }
    }
    statements.extend(deferred_fks);
    statements.push(format!(
        "INSERT INTO {ledger} (name) VALUES ('{}')",
        mig.name.replace('\'', "''")
    ));
    if mig.atomic {
        statements.push("COMMIT".to_string());
    }
    Ok(MigrationPreview {
        name: mig.name.clone(),
        atomic: mig.atomic,
        statements,
    })
}

async fn apply_atomic(
    pool: &PgPool,
    mig: &Migration,
    ledger: &str,
) -> Result<(), MigrateError> {
    tracing::info!(migration = %mig.name, "applying (atomic)");
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
    sqlx::query(&format!("INSERT INTO {ledger} (name) VALUES ($1)"))
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
    Builder::default().migrate_to(pool, dir, target).await
}

async fn migrate_to_with_ledger(
    pool: &PgPool,
    dir: &Path,
    target: &str,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger_for(pool, ledger).await?;
    with_migrate_lock(pool, async {
        let all = file::list_dir(dir)?;
        let applied = applied_set_for(pool, ledger).await?;

        if target == "zero" {
            return unapply_all_in_order(pool, dir, &all, &applied, ledger).await;
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
                    apply_one(pool, &mig, ledger).await?;
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
                            apply_one(pool, &mig, ledger).await?;
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
                            unapply_locked(pool, dir, &mig.name, ledger).await?;
                            touched.push(mig);
                        }
                    }
                }
            }
        }
        Ok(touched)
    })
    .await
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
    Builder::default().migrate_embedded(pool, embedded).await
}

async fn migrate_embedded_with_ledger(
    pool: &PgPool,
    embedded: &[(&str, &str)],
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger_for(pool, ledger).await?;
    with_migrate_lock(pool, async {
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
        file::validate_chain(&all, "embedded slice")?;

        let applied = applied_set_for(pool, ledger).await?;
        let pending: Vec<Migration> = all
            .into_iter()
            .filter(|m| !applied.contains(&m.name))
            .collect();

        let mut newly = Vec::with_capacity(pending.len());
        for mig in pending {
            apply_one(pool, &mig, ledger).await?;
            newly.push(mig);
        }
        Ok(newly)
    })
    .await
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
    Builder::default().downgrade(pool, dir, steps).await
}

async fn downgrade_with_ledger(
    pool: &PgPool,
    dir: &Path,
    steps: usize,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    if steps == 0 {
        return Ok(Vec::new());
    }
    ensure_ledger_for(pool, ledger).await?;
    with_migrate_lock(pool, async {
        let all = file::list_dir(dir)?;
        let applied = applied_set_for(pool, ledger).await?;

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
            unapply_locked(pool, dir, &mig.name, ledger).await?;
            touched.push(mig);
        }
        Ok(touched)
    })
    .await
}

async fn apply_one(pool: &PgPool, mig: &Migration, ledger: &str) -> Result<(), MigrateError> {
    if mig.atomic {
        apply_atomic(pool, mig, ledger).await
    } else {
        apply_loose(pool, mig, ledger).await
    }
}

async fn unapply_all_in_order(
    pool: &PgPool,
    dir: &Path,
    all: &[Migration],
    applied: &HashSet<String>,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    let mut to_unapply: Vec<Migration> = all
        .iter()
        .filter(|m| applied.contains(&m.name))
        .cloned()
        .collect();
    to_unapply.reverse();
    let mut touched = Vec::with_capacity(to_unapply.len());
    for mig in to_unapply {
        // Caller already holds the migrate lock; use `unapply_locked`
        // to avoid re-acquiring (which would deadlock on a different
        // pooled connection / session).
        unapply_locked(pool, dir, &mig.name, ledger).await?;
        touched.push(mig);
    }
    Ok(touched)
}

/// Roll back a single applied migration.
///
/// Loads `dir/{name}.json`, looks up its predecessor (or empty for
/// the first migration) for snapshot context, computes the inverse
/// op list via [`super::invert::invert`], and executes it in a
/// transaction (or loose if the original `atomic: false`). Removes
/// the entry from `__rustango_migrations__` on success.
///
/// **Refuses to unapply a non-head migration** — leaving an applied
/// migration newer than the rolled-back one would put the schema in
/// an inconsistent state (the newer one still thinks its predecessor
/// is in place). Use [`downgrade`] or [`migrate_to`] for ordered
/// rollback, or [`unapply_force`] to bypass.
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
/// * [`MigrateError::Validation`] — non-head target, irreversible
///   op, missing migration file, missing predecessor.
/// * [`MigrateError::Driver`] — SQL failure during rollback.
pub async fn unapply(pool: &PgPool, dir: &Path, name: &str) -> Result<Migration, MigrateError> {
    Builder::default().unapply(pool, dir, name).await
}

async fn unapply_with_ledger(
    pool: &PgPool,
    dir: &Path,
    name: &str,
    ledger: &str,
) -> Result<Migration, MigrateError> {
    ensure_ledger_for(pool, ledger).await?;
    with_migrate_lock(pool, async {
        check_is_head(pool, dir, name, ledger).await?;
        unapply_locked(pool, dir, name, ledger).await
    })
    .await
}

/// Roll back any applied migration, even out of order.
///
/// Same body as [`unapply`] but skips the head check — the caller
/// accepts responsibility for the resulting schema state. Use only
/// when you genuinely need to drop an arbitrary applied migration
/// (e.g. surgical correction of a bad migration mid-history); in
/// most cases [`downgrade`] or [`migrate_to`] is what you want.
///
/// # Errors
/// As [`unapply`], minus the head-mismatch check.
pub async fn unapply_force(
    pool: &PgPool,
    dir: &Path,
    name: &str,
) -> Result<Migration, MigrateError> {
    Builder::default().unapply_force(pool, dir, name).await
}

async fn unapply_force_with_ledger(
    pool: &PgPool,
    dir: &Path,
    name: &str,
    ledger: &str,
) -> Result<Migration, MigrateError> {
    ensure_ledger_for(pool, ledger).await?;
    with_migrate_lock(pool, unapply_locked(pool, dir, name, ledger)).await
}

/// Verify `name` is the lex-greatest currently-applied migration.
/// Silent pass-through if the migration isn't applied at all — that
/// case will surface as a clearer error from `unapply_locked`
/// ("migration not found in dir" or similar).
async fn check_is_head(
    pool: &PgPool,
    dir: &Path,
    name: &str,
    ledger: &str,
) -> Result<(), MigrateError> {
    let applied = applied_set_for(pool, ledger).await?;
    if !applied.contains(name) {
        return Ok(());
    }
    let all = file::list_dir(dir)?;
    let head = all
        .iter()
        .rev()
        .find(|m| applied.contains(&m.name))
        .map(|m| m.name.as_str());
    match head {
        Some(h) if h == name => Ok(()),
        Some(h) => Err(MigrateError::Validation(format!(
            "refusing to unapply `{name}` out of order: current head is `{h}`. \
             Use `downgrade(pool, dir, n)` / `migrate_to(pool, dir, target)` for \
             ordered rollback, or `unapply_force` to bypass.",
        ))),
        None => Ok(()),
    }
}

/// Body of [`unapply`] without acquiring the migrate lock — for
/// reuse by `migrate_to` and `downgrade`, which already hold the
/// lock for the whole operation. Acquiring the lock recursively on
/// a different pooled connection would block forever (each
/// `pool.acquire()` is a fresh session).
async fn unapply_locked(
    pool: &PgPool,
    dir: &Path,
    name: &str,
    ledger: &str,
) -> Result<Migration, MigrateError> {
    let all = file::list_dir(dir)?;
    let target = all
        .iter()
        .find(|m| m.name == name)
        .cloned()
        .ok_or_else(|| {
            MigrateError::Validation(format!("migration `{name}` not found in {}", dir.display()))
        })?;

    let prev_snapshot = match &target.prev {
        None => SchemaSnapshot { tables: vec![], m2m_tables: vec![], indexes: vec![] },
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
        unapply_atomic(pool, &target, &inverted, &prev_snapshot, ledger).await?;
    } else {
        unapply_loose(pool, &target, &inverted, &prev_snapshot, ledger).await?;
    }

    Ok(target)
}

async fn unapply_atomic(
    pool: &PgPool,
    target: &Migration,
    inverted: &[Operation],
    snapshot: &SchemaSnapshot,
    ledger: &str,
) -> Result<(), MigrateError> {
    tracing::info!(migration = %target.name, "unapplying (atomic)");
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
    sqlx::query(&format!("DELETE FROM {ledger} WHERE name = $1"))
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
    ledger: &str,
) -> Result<(), MigrateError> {
    tracing::info!(migration = %target.name, "unapplying (non-atomic)");
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
    sqlx::query(&format!("DELETE FROM {ledger} WHERE name = $1"))
        .bind(&target.name)
        .execute(pool)
        .await?;
    Ok(())
}

async fn apply_loose(pool: &PgPool, mig: &Migration, ledger: &str) -> Result<(), MigrateError> {
    tracing::info!(migration = %mig.name, "applying (non-atomic)");
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
    sqlx::query(&format!("INSERT INTO {ledger} (name) VALUES ($1)"))
        .bind(&mig.name)
        .execute(pool)
        .await?;
    Ok(())
}
