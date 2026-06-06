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
use crate::sql::sqlx;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::Row;
// PG-typed shims below import these; sqlite/mysql-only builds get
// just the `_pool` entry points.
#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;

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
    /// PG-typed back-compat. For non-PG migrations, call
    /// [`migrate_pool`] directly.
    ///
    /// # Errors
    /// As [`migrate`].
    #[cfg(feature = "postgres")]
    pub async fn migrate(&self, pool: &PgPool, dir: &Path) -> Result<Vec<Migration>, MigrateError> {
        migrate_with_ledger(pool, dir, self.ledger).await
    }

    /// As [`migrate_to`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`migrate_to`].
    #[cfg(feature = "postgres")]
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
    #[cfg(feature = "postgres")]
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
    #[cfg(feature = "postgres")]
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
    #[cfg(feature = "postgres")]
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
    #[cfg(feature = "postgres")]
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
    #[cfg(feature = "postgres")]
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
    #[cfg(feature = "postgres")]
    pub async fn applied_set(&self, pool: &PgPool) -> Result<HashSet<String>, MigrateError> {
        applied_set_for(pool, self.ledger).await
    }

    /// As [`ensure_ledger`], with this builder's ledger.
    ///
    /// # Errors
    /// As [`ensure_ledger`].
    #[cfg(feature = "postgres")]
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
#[cfg(any(feature = "postgres", feature = "mysql"))]
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
/// PG-typed back-compat; for non-PG use [`apply_all_pool`].
///
/// # Errors
/// Returns [`MigrateError`] for any sqlx failure (connection, syntax,
/// constraint violation).
#[cfg(feature = "postgres")]
pub async fn apply_all(pool: &PgPool) -> Result<(), MigrateError> {
    use crate::signals::migrate::{
        send_post_migrate, send_pre_migrate, PostMigrateContext, PreMigrateContext,
    };
    send_pre_migrate(PreMigrateContext {
        source: "apply_all",
    })
    .await;
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
    // #411 — post_migrate fires once after the bootstrap walk
    // completes successfully.
    send_post_migrate(PostMigrateContext {
        source: "apply_all",
        applied: Vec::new(),
    })
    .await;
    Ok(())
}

/// `DROP TABLE IF EXISTS … CASCADE` for every registered model. CASCADE
/// makes order irrelevant — FKs go away with the parent table.
/// PG-typed back-compat; for non-PG use [`drop_all_pool`].
///
/// # Errors
/// Returns [`MigrateError`] for any sqlx failure.
#[cfg(feature = "postgres")]
pub async fn drop_all(pool: &PgPool) -> Result<(), MigrateError> {
    for model in registered_models() {
        let sql = ddl::drop_table_sql(model, /* if_exists */ true, /* cascade */ true);
        sqlx::query(&sql).execute(pool).await?;
    }
    Ok(())
}

/// `apply_all` against either backend. Equivalent to [`apply_all`] but
/// takes [`crate::sql::Pool`] and dispatches per backend — uses the
/// dialect-aware DDL emitters from
/// [`crate::migrate::ddl::create_table_sql_with_dialect`] +
/// [`crate::migrate::ddl::create_constraints_sql_with_dialect`], so
/// MySQL gets backticks + `TINYINT(1)` + `BIGINT AUTO_INCREMENT` etc.
///
/// Useful for dev bootstrap, ephemeral test databases, and one-shot
/// CLI tools — for production schema evolution use the file-based
/// [`migrate`] runner (still PG-only; bi-dialect ledger path lands
/// in a follow-up batch).
///
/// # Errors
/// As [`apply_all`].
pub async fn apply_all_pool(pool: &crate::sql::Pool) -> Result<(), MigrateError> {
    use crate::signals::migrate::{
        send_post_migrate, send_pre_migrate, PostMigrateContext, PreMigrateContext,
    };
    send_pre_migrate(PreMigrateContext {
        source: "apply_all_pool",
    })
    .await;
    let dialect = pool.dialect();
    let models = registered_models();
    for model in &models {
        let sql = ddl::create_table_sql_with_dialect(dialect, model);
        crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
    }
    for model in &models {
        for sql in ddl::create_constraints_sql_with_dialect(dialect, model) {
            crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
        }
    }
    // #450 — post-hoc `COMMENT ON COLUMN` for dialects that need it
    // (Postgres). MySQL already inlined comments in CREATE TABLE;
    // SQLite returns an empty vec (no native column comments).
    for model in &models {
        for sql in ddl::column_comment_statements_with_dialect(dialect, model) {
            crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
        }
    }
    // Django Meta.db_table_comment — same shape as column-level:
    // PG emits a post-hoc `COMMENT ON TABLE`, MySQL inlined it in
    // CREATE TABLE, SQLite emits nothing.
    for model in &models {
        for sql in ddl::table_comment_statements_with_dialect(dialect, model) {
            crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
        }
    }
    // #411 — post_migrate fires once after the bootstrap walk
    // completes. `applied` is empty because apply_all_pool doesn't
    // carry per-migration names — it walks the model inventory.
    send_post_migrate(PostMigrateContext {
        source: "apply_all_pool",
        applied: Vec::new(),
    })
    .await;
    Ok(())
}

/// `drop_all` against either backend. Equivalent to [`drop_all`] but
/// takes [`crate::sql::Pool`].
///
/// MySQL caveat: `DROP TABLE … CASCADE` is rejected by MySQL's parser
/// (MySQL drops cascade FK constraints automatically and doesn't take
/// the keyword). For now this routes the cascade flag through PG only;
/// MySQL gets `DROP TABLE IF EXISTS` without it. A future batch will
/// add `Dialect::supports_drop_cascade()` to gate the keyword cleanly.
///
/// # Errors
/// As [`drop_all`].
pub async fn drop_all_pool(pool: &crate::sql::Pool) -> Result<(), MigrateError> {
    let dialect = pool.dialect();
    // Cascade only emitted for PG — MySQL parses it as syntax error.
    let cascade = dialect.name() == "postgres";
    for model in registered_models() {
        let sql =
            ddl::drop_table_sql_with_dialect(dialect, model, /* if_exists */ true, cascade);
        crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
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
#[cfg(feature = "postgres")]
pub async fn migrate(pool: &PgPool, dir: &Path) -> Result<Vec<Migration>, MigrateError> {
    Builder::default().migrate(pool, dir).await
}

#[cfg(feature = "postgres")]
async fn migrate_with_ledger(
    pool: &PgPool,
    dir: &Path,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    use crate::signals::migrate::{
        send_post_migrate, send_pre_migrate, PostMigrateContext, PreMigrateContext,
    };
    send_pre_migrate(PreMigrateContext { source: "migrate" }).await;
    ensure_ledger_for(pool, ledger).await?;
    let newly = with_migrate_lock(pool, async {
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
    .await?;
    // #411 — post_migrate fires once after the file-based migrate
    // session completes. `applied` lists newly-applied migration
    // names (empty when everything was already applied).
    send_post_migrate(PostMigrateContext {
        source: "migrate",
        applied: newly.iter().map(|m| m.name.clone()).collect(),
    })
    .await;
    Ok(newly)
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
#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
pub async fn applied_set(pool: &PgPool) -> Result<HashSet<String>, MigrateError> {
    applied_set_for(pool, LEDGER_TABLE).await
}

#[cfg(feature = "postgres")]
async fn applied_set_for(pool: &PgPool, ledger: &str) -> Result<HashSet<String>, MigrateError> {
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
#[cfg(feature = "postgres")]
pub async fn ensure_ledger(pool: &PgPool) -> Result<(), MigrateError> {
    ensure_ledger_for(pool, LEDGER_TABLE).await
}

#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
pub async fn migrate_dry_run(
    pool: &PgPool,
    dir: &Path,
) -> Result<Vec<MigrationPreview>, MigrateError> {
    Builder::default().migrate_dry_run(pool, dir).await
}

#[cfg(feature = "postgres")]
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

/// Django-shape `sqlmigrate <name>` — Compute the SQL the named
/// migration would emit when applied, without touching the database.
/// Pure file I/O + render — no ledger read required.
///
/// Issue #345. Use from `manage sqlmigrate <name>`.
///
/// # Errors
/// - [`MigrateError::Validation`] when `name` is not present in `dir`.
/// - Any IO / parse error from [`file::list_dir`].
pub fn sqlmigrate_one(dir: &Path, name: &str) -> Result<MigrationPreview, MigrateError> {
    let all = file::list_dir(dir)?;
    let mig = all.into_iter().find(|m| m.name == name).ok_or_else(|| {
        MigrateError::Validation(format!("migration `{name}` not found in {}", dir.display()))
    })?;
    preview_migration(&mig, LEDGER_TABLE)
}

/// #347 — invoke a named migration callback. Looks the name up in
/// the inventory registry and `await`s the future. Unknown names
/// surface as `MigrateError::Validation` so the operator gets a clear
/// pointer to the missing `register_migration_callback!` call.
async fn invoke_migration_callback(
    op: &crate::migrate::file::CallbackOp,
    pool: crate::sql::Pool,
) -> Result<(), MigrateError> {
    let cb = crate::migrate::callbacks::find(&op.name).ok_or_else(|| {
        MigrateError::Validation(format!(
            "migration callback `{}` is not registered — \
             call `rustango::register_migration_callback!(\"{0}\", …)` \
             at startup",
            op.name,
        ))
    })?;
    (cb.forward)(pool).await
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
            Operation::Callback(c) => {
                // #347 — RunPython preview. The callback body isn't
                // SQL, so the preview emits a comment marker so
                // operators can see WHERE the side effect lands in
                // the apply order.
                statements.push(format!("-- RunPython: {}", c.name));
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

#[cfg(feature = "postgres")]
async fn apply_atomic(pool: &PgPool, mig: &Migration, ledger: &str) -> Result<(), MigrateError> {
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
            Operation::Callback(c) => {
                // #347 — RunPython runs OUTSIDE the migration's tx
                // because our callback signature takes an owned `Pool`
                // (drives its own connections). Document the limitation
                // — operators who want atomicity should set
                // `atomic: false` on the migration and manage their
                // own transactions.
                invoke_migration_callback(c, pool.clone().into()).await?;
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
#[cfg(feature = "postgres")]
pub async fn migrate_to(
    pool: &PgPool,
    dir: &Path,
    target: &str,
) -> Result<Vec<Migration>, MigrateError> {
    Builder::default().migrate_to(pool, dir, target).await
}

#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
pub async fn migrate_embedded(
    pool: &PgPool,
    embedded: &[(&str, &str)],
) -> Result<Vec<Migration>, MigrateError> {
    Builder::default().migrate_embedded(pool, embedded).await
}

#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
pub async fn downgrade(
    pool: &PgPool,
    dir: &Path,
    steps: usize,
) -> Result<Vec<Migration>, MigrateError> {
    Builder::default().downgrade(pool, dir, steps).await
}

#[cfg(feature = "postgres")]
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

#[cfg(feature = "postgres")]
async fn apply_one(pool: &PgPool, mig: &Migration, ledger: &str) -> Result<(), MigrateError> {
    if mig.atomic {
        apply_atomic(pool, mig, ledger).await
    } else {
        apply_loose(pool, mig, ledger).await
    }
}

#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
pub async fn unapply(pool: &PgPool, dir: &Path, name: &str) -> Result<Migration, MigrateError> {
    Builder::default().unapply(pool, dir, name).await
}

#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
pub async fn unapply_force(
    pool: &PgPool,
    dir: &Path,
    name: &str,
) -> Result<Migration, MigrateError> {
    Builder::default().unapply_force(pool, dir, name).await
}

#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
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
#[cfg(feature = "postgres")]
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
        None => SchemaSnapshot {
            tables: vec![],
            m2m_tables: vec![],
            indexes: vec![],
            checks: vec![],
            excludes: vec![],
        },
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

#[cfg(feature = "postgres")]
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
            Operation::Callback(c) => {
                // #347 — callback runs OUTSIDE the surrounding tx; see
                // `invoke_migration_callback` doc.
                invoke_migration_callback(c, pool.clone().into()).await?;
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

#[cfg(feature = "postgres")]
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
            Operation::Callback(c) => {
                // #347 — non-tx PG path; pool is &PgPool, convert to
                // the Pool enum via the From impl.
                invoke_migration_callback(c, pool.clone().into()).await?;
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

#[cfg(feature = "postgres")]
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
            Operation::Callback(c) => {
                // #347 — non-tx PG path; pool is &PgPool, convert to
                // the Pool enum via the From impl.
                invoke_migration_callback(c, pool.clone().into()).await?;
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

// ====================================================================
// `&Pool` file-based ledger runner — v0.23.0-batch12
// ====================================================================
//
// Bi-dialect counterpart to `migrate(&PgPool, dir)`. Same semantics
// (skip already-applied migrations from the ledger, apply each in a
// transaction by default), same default ledger table name. Skipped
// in this batch:
//
// - Advisory locks. PG and MySQL emit different lock-name shapes
//   (i64 vs string) and the bind needs per-backend dispatch — that's
//   batch 13. Until then, concurrent `migrate_pool` calls against the
//   same DB can race; single-process bootstrap is safe.
// - migrate_to_pool / unapply_pool / downgrade_pool / migrate_dry_run_pool —
//   the harder direction-aware paths land in batch 13+ once the
//   advisory lock dispatch is in place.
// - Per-Builder customization on the `_pool` path. Default ledger
//   only for batch 12.

/// Ensure the default ledger table (`__rustango_migrations__`) exists
/// on either backend. Idempotent — re-running on an existing ledger
/// is a no-op.
///
/// Backend-specific ledger DDL (the `applied_at` column type differs):
/// - Postgres: `applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`
/// - MySQL: `applied_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)`
///
/// # Errors
/// Returns [`MigrateError::Exec`] for any executor / driver failure.
pub async fn ensure_ledger_pool(pool: &crate::sql::Pool) -> Result<(), MigrateError> {
    ensure_ledger_pool_with_ledger(pool, LEDGER_TABLE).await
}

/// Ensure a custom-named ledger table exists. Pair with the other
/// `*_pool_with_ledger` entry points to operate against a non-default
/// ledger. Issue #146 — operator-controlled ledger naming.
pub async fn ensure_ledger_pool_with_ledger(
    pool: &crate::sql::Pool,
    ledger: &str,
) -> Result<(), MigrateError> {
    let dialect_name = pool.dialect().name();
    let timestamp_col = match dialect_name {
        "postgres" => "TIMESTAMPTZ NOT NULL DEFAULT NOW()",
        "mysql" => "DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)",
        // SQLite has no native TIMESTAMP type — TEXT with affinity
        // and `CURRENT_TIMESTAMP` (UTC, ISO-8601 to second precision)
        // is the conventional shape. Sufficient for ledger ordering.
        "sqlite" => "TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
        // Future dialects: a `Dialect::current_timestamp_default()` +
        // `Dialect::timestamp_type()` pair would let this branch go
        // away. For now the runner only knows the backends rustango
        // ships against.
        other => {
            return Err(MigrateError::Validation(format!(
                "ensure_ledger_pool: unrecognized dialect `{other}`"
            )));
        }
    };
    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {ledger} (\
         name VARCHAR(255) PRIMARY KEY, \
         applied_at {timestamp_col})"
    );
    crate::sql::raw_execute_pool(pool, &create_sql, ::std::vec::Vec::new()).await?;
    Ok(())
}

/// Set of migration names already recorded in the default ledger
/// against either backend.
///
/// # Errors
/// Returns [`MigrateError::Exec`] for any read failure (including a
/// missing ledger table — call [`ensure_ledger_pool`] first).
pub async fn applied_set_pool(pool: &crate::sql::Pool) -> Result<HashSet<String>, MigrateError> {
    applied_set_pool_with_ledger(pool, LEDGER_TABLE).await
}

/// Read the applied-migration name set from a custom-named ledger
/// table. Pairs with [`ensure_ledger_pool_with_ledger`] / the other
/// `*_pool_with_ledger` entry points. Issue #146.
pub async fn applied_set_pool_with_ledger(
    pool: &crate::sql::Pool,
    ledger: &str,
) -> Result<HashSet<String>, MigrateError> {
    let sql = format!("SELECT name FROM {ledger}");
    // #561 — was 3-arm `match pool` doing byte-identical
    // `try_get::<String, _>("name")` loops. The
    // `raw_query_pool::<(String,)>` positional tuple decode pulls
    // the single column on every backend via the `Maybe*FromRow`
    // blanket impls. `ExecError` rides in via `MigrateError::Exec`'s
    // `#[from]` impl.
    let rows: Vec<(String,)> = crate::sql::raw_query_pool(&sql, Vec::new(), pool).await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// Apply every pending migration in `dir` against either backend.
/// Each migration runs in its own transaction unless its `atomic`
/// field is `false` (e.g. `CREATE INDEX CONCURRENTLY`, which neither
/// PG nor MySQL allow inside a transaction).
///
/// Skips files already recorded in the ledger. Returns the migrations
/// that were newly applied.
///
/// **Concurrency caveat (batch 12):** no advisory lock yet — peers
/// running `migrate_pool` against the same DB simultaneously can both
/// pass the `applied_set` check and try to apply the same file. The
/// ledger PRIMARY KEY constraint catches the second writer's INSERT
/// and rolls its transaction back, but you'll see noisy errors. Lock
/// dispatch lands in batch 13.
///
/// # Errors
/// As [`migrate`].
pub async fn migrate_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
) -> Result<Vec<Migration>, MigrateError> {
    migrate_pool_with_ledger(pool, dir, LEDGER_TABLE).await
}

/// Apply every pending migration in `dir` against a custom-named
/// ledger table. Sibling of [`migrate_pool`] with operator-supplied
/// ledger name (issue #146). Use for multi-tenant / multi-app
/// deployments where two migration directories share a database and
/// must not collide on the default `__rustango_migrations__`
/// bookkeeping table.
///
/// # Errors
/// As [`migrate_pool`].
pub async fn migrate_pool_with_ledger(
    pool: &crate::sql::Pool,
    dir: &Path,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger_pool_with_ledger(pool, ledger).await?;
    with_migrate_lock_pool(pool, async {
        let all = file::list_dir(dir)?;
        let applied = applied_set_pool_with_ledger(pool, ledger).await?;
        let pending: Vec<Migration> = all
            .into_iter()
            .filter(|m| !applied.contains(&m.name))
            .collect();

        let mut newly = Vec::with_capacity(pending.len());
        for mig in pending {
            if mig.atomic {
                apply_atomic_pool(pool, &mig, ledger).await?;
            } else {
                apply_nonatomic_pool(pool, &mig, ledger).await?;
            }
            newly.push(mig);
        }
        Ok(newly)
    })
    .await
}

/// Hold the migrate session-scoped advisory lock while `body` runs,
/// then release. Bi-dialect counterpart of [`with_migrate_lock`] —
/// dispatches the lock acquire/release SQL through the pool's dialect.
///
/// Backend-specific bind shapes:
/// - **Postgres** — `pg_advisory_lock($1)` takes an `i64`; we bind
///   [`MIGRATE_LOCK_KEY`] (the same key the legacy PgPool runner
///   uses, so the two paths coordinate).
/// - **MySQL** — `GET_LOCK(?, -1)` takes a `VARCHAR` lock name; we
///   bind `format!("rustango_migrate_{:x}", MIGRATE_LOCK_KEY)` so
///   the name is stable, deterministic, and namespaced (MySQL
///   `GET_LOCK` is global to the server, not scoped per database).
///
/// The lock is acquired on a checked-out connection and held until
/// `body` returns; release happens on the same connection so MySQL's
/// connection-scoped `GET_LOCK` semantics work correctly.
async fn with_migrate_lock_pool<F, R>(pool: &crate::sql::Pool, body: F) -> Result<R, MigrateError>
where
    F: std::future::Future<Output = Result<R, MigrateError>>,
{
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut lock_conn = pg.acquire().await?;
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(MIGRATE_LOCK_KEY)
                .execute(&mut *lock_conn)
                .await?;
            let result = body.await;
            // Best-effort release. PG releases on session close anyway,
            // so a failed unlock can't permanently deadlock peers.
            let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(MIGRATE_LOCK_KEY)
                .execute(&mut *lock_conn)
                .await;
            result
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let lock_name = format!("rustango_migrate_{:x}", MIGRATE_LOCK_KEY);
            let mut lock_conn = my.acquire().await?;
            sqlx::query("SELECT GET_LOCK(?, -1)")
                .bind(&lock_name)
                .execute(&mut *lock_conn)
                .await?;
            let result = body.await;
            // Best-effort release. MySQL releases on connection close,
            // so a failed RELEASE_LOCK can't permanently deadlock peers.
            let _ = sqlx::query("SELECT RELEASE_LOCK(?)")
                .bind(&lock_name)
                .execute(&mut *lock_conn)
                .await;
            result
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(_) => {
            // SQLite has no advisory-lock primitive comparable to PG's
            // `pg_advisory_lock` or MySQL's `GET_LOCK`. The single-writer
            // semantics of SQLite (and the typical single-process
            // deployment shape) make migration coordination unnecessary
            // — concurrent migrations would serialize on the database
            // file lock anyway. Run the body without an additional lock.
            body.await
        }
    }
}

/// Apply one migration inside a transaction. Both backends support
/// the same `BEGIN`/`COMMIT`/`ROLLBACK` shape, but sqlx's `Transaction<DB>`
/// is generic over the backend so the body is inlined per-arm rather
/// than factored — `Executor<Database = sqlx::Postgres>` and
/// `Executor<Database = sqlx::MySql>` can't share a single generic
/// function without a Database-erased shim trait that doesn't ship
/// in sqlx.
async fn apply_atomic_pool(
    pool: &crate::sql::Pool,
    mig: &Migration,
    ledger: &str,
) -> Result<(), MigrateError> {
    tracing::info!(migration = %mig.name, "applying (atomic, _pool)");
    let dialect = pool.dialect();
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in &mig.forward {
                match op {
                    Operation::Schema(change) => {
                        let batch = super::diff::render_changes_split_with_dialect(
                            std::slice::from_ref(change),
                            &mig.snapshot,
                            dialect,
                        )
                        .map_err(MigrateError::Validation)?;
                        for stmt in batch.immediate {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => {
                        // #347 — see `invoke_migration_callback` doc.
                        invoke_migration_callback(c, pool.clone().into()).await?;
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
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            // ⚠ MySQL atomic migrations have a caveat that PG / SQLite
            // don't: MySQL silently auto-COMMITs the current
            // transaction on every DDL statement (CREATE TABLE,
            // ALTER TABLE, DROP TABLE, CREATE INDEX, etc. — the
            // "implicit commit before/after statement" list in
            // https://dev.mysql.com/doc/refman/8.0/en/implicit-commit.html).
            // The BEGIN we issue below establishes a tx that subsequent
            // `Operation::Data(RunSQL)` statements DO participate in,
            // but every `Operation::Schema(change)` auto-commits and
            // breaks atomicity. The COMMIT at the end is a no-op for
            // any DDL emitted above.
            //
            // In practice this means: if a migration has 5 DDL ops and
            // op #3 fails, ops #1+#2 are already committed and can't be
            // rolled back. The migration ends up half-applied; operators
            // have to manually un-do the partially-applied DDL OR fix
            // the migration to be re-runnable from where it failed.
            //
            // This is a MySQL engine limitation, not a rustango bug,
            // and matches Django's `migrate` behavior against MySQL
            // (Django docs note the same caveat). Tracked in #559.
            // The runner emits a `tracing::warn!` so operators see
            // the caveat in logs when they invoke `atomic: true` on
            // MySQL.
            tracing::warn!(
                migration = %mig.name,
                "MySQL silently auto-commits on every DDL statement (CREATE/ALTER/DROP/INDEX); \
                 the atomic-migration wrapper only protects RunSQL/RunPython operations \
                 between DDL ops. A failure mid-DDL leaves the migration partially applied \
                 and requires manual recovery. See migrate/runner.rs::apply_atomic_pool for \
                 details. Tracked in #559."
            );
            let mut tx = my.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in &mig.forward {
                match op {
                    Operation::Schema(change) => {
                        let batch = super::diff::render_changes_split_with_dialect(
                            std::slice::from_ref(change),
                            &mig.snapshot,
                            dialect,
                        )
                        .map_err(MigrateError::Validation)?;
                        for stmt in batch.immediate {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => {
                        // #347 — see `invoke_migration_callback` doc.
                        invoke_migration_callback(c, pool.clone().into()).await?;
                    }
                }
            }
            for stmt in deferred_fks {
                sqlx::query(&stmt).execute(&mut *tx).await?;
            }
            sqlx::query(&format!("INSERT INTO {ledger} (name) VALUES (?)"))
                .bind(&mig.name)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            // SQLite supports the same BEGIN/COMMIT/ROLLBACK shape as
            // PG/MySQL, with `?` placeholders. Mirror the PG arm.
            let mut tx = sq.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in &mig.forward {
                match op {
                    Operation::Schema(change) => {
                        let batch = super::diff::render_changes_split_with_dialect(
                            std::slice::from_ref(change),
                            &mig.snapshot,
                            dialect,
                        )
                        .map_err(MigrateError::Validation)?;
                        for stmt in batch.immediate {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => {
                        // #347 — see `invoke_migration_callback` doc.
                        invoke_migration_callback(c, pool.clone().into()).await?;
                    }
                }
            }
            for stmt in deferred_fks {
                sqlx::query(&stmt).execute(&mut *tx).await?;
            }
            sqlx::query(&format!("INSERT INTO {ledger} (name) VALUES (?)"))
                .bind(&mig.name)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
    }
    Ok(())
}

/// Apply one migration without a transaction (the file's `atomic`
/// field is `false` — typically because it contains `CREATE INDEX
/// CONCURRENTLY` which neither backend allows inside a transaction).
async fn apply_nonatomic_pool(
    pool: &crate::sql::Pool,
    mig: &Migration,
    ledger: &str,
) -> Result<(), MigrateError> {
    tracing::info!(migration = %mig.name, "applying (non-atomic, _pool)");
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let batch = super::diff::render_changes_split_with_dialect(
                    std::slice::from_ref(change),
                    &mig.snapshot,
                    pool.dialect(),
                )
                .map_err(MigrateError::Validation)?;
                for stmt in batch.immediate {
                    crate::sql::raw_execute_pool(pool, &stmt, ::std::vec::Vec::new()).await?;
                }
                deferred_fks.extend(batch.deferred_fks);
            }
            Operation::Data(d) => {
                crate::sql::raw_execute_pool(pool, &d.sql, ::std::vec::Vec::new()).await?;
            }
            Operation::Callback(c) => {
                // #347 — non-tx context; pass the pool directly.
                invoke_migration_callback(c, pool.clone()).await?;
            }
        }
    }
    for stmt in deferred_fks {
        crate::sql::raw_execute_pool(pool, &stmt, ::std::vec::Vec::new()).await?;
    }
    let placeholder = pool.dialect().placeholder(1);
    let insert_sql = format!("INSERT INTO {ledger} (name) VALUES ({placeholder})");
    crate::sql::raw_execute_pool(
        pool,
        &insert_sql,
        ::std::vec![crate::core::SqlValue::String(mig.name.clone())],
    )
    .await?;
    Ok(())
}

// ====================================================================
// Direction-aware `_pool` runners — v0.23.0-batch14
// ====================================================================
//
// `migrate_to_pool` / `unapply_pool` / `unapply_force_pool` /
// `downgrade_pool` / `migrate_dry_run_pool` — bi-dialect counterparts
// to the existing PgPool functions. Same semantics, advisory-locked
// via `with_migrate_lock_pool` (batch 13).
//
// `migrate_embedded_pool` follows the same pattern but isn't yet
// emitted — the embed_migrations! macro and its callers are
// PgPool-bound and migrating them is a separate concern.

/// Move the database to a specific migration target — bi-dialect
/// counterpart of [`migrate_to`].
///
/// # Errors
/// As [`migrate_to`].
pub async fn migrate_to_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
    target: &str,
) -> Result<Vec<Migration>, MigrateError> {
    migrate_to_pool_with_ledger(pool, dir, target, LEDGER_TABLE).await
}

/// Migrate to a specific target against a custom-named ledger.
/// Sibling of [`migrate_to_pool`] (issue #146).
///
/// # Errors
/// As [`migrate_to_pool`].
pub async fn migrate_to_pool_with_ledger(
    pool: &crate::sql::Pool,
    dir: &Path,
    target: &str,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger_pool_with_ledger(pool, ledger).await?;
    with_migrate_lock_pool(pool, async {
        let all = file::list_dir(dir)?;
        let applied = applied_set_pool_with_ledger(pool, ledger).await?;

        if target == "zero" {
            return unapply_all_in_order_pool(pool, dir, &all, &applied, ledger).await;
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
                for mig in all.into_iter().filter(|m| m.name.as_str() <= target) {
                    apply_one_pool(pool, &mig, ledger).await?;
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
                            apply_one_pool(pool, &mig, ledger).await?;
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
                            unapply_locked_pool(pool, dir, &mig.name, ledger).await?;
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

/// Step back `steps` applied migrations against either backend.
///
/// # Errors
/// As [`downgrade`].
pub async fn downgrade_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
    steps: usize,
) -> Result<Vec<Migration>, MigrateError> {
    downgrade_pool_with_ledger(pool, dir, steps, LEDGER_TABLE).await
}

/// Roll back `steps` migrations against a custom-named ledger.
/// Sibling of [`downgrade_pool`] (issue #146).
///
/// # Errors
/// As [`downgrade_pool`].
pub async fn downgrade_pool_with_ledger(
    pool: &crate::sql::Pool,
    dir: &Path,
    steps: usize,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    if steps == 0 {
        return Ok(Vec::new());
    }
    ensure_ledger_pool_with_ledger(pool, ledger).await?;
    with_migrate_lock_pool(pool, async {
        let all = file::list_dir(dir)?;
        let applied = applied_set_pool_with_ledger(pool, ledger).await?;

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
            unapply_locked_pool(pool, dir, &mig.name, ledger).await?;
            touched.push(mig);
        }
        Ok(touched)
    })
    .await
}

/// Roll back a single applied migration against either backend.
/// Refuses non-head targets (use [`downgrade_pool`] /
/// [`migrate_to_pool`] for ordered rollback, or
/// [`unapply_force_pool`] to bypass).
///
/// # Errors
/// As [`unapply`].
pub async fn unapply_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
    name: &str,
) -> Result<Migration, MigrateError> {
    unapply_pool_with_ledger(pool, dir, name, LEDGER_TABLE).await
}

/// Unapply a single named migration against a custom-named ledger.
/// Sibling of [`unapply_pool`] (issue #146).
///
/// # Errors
/// As [`unapply_pool`].
pub async fn unapply_pool_with_ledger(
    pool: &crate::sql::Pool,
    dir: &Path,
    name: &str,
    ledger: &str,
) -> Result<Migration, MigrateError> {
    ensure_ledger_pool_with_ledger(pool, ledger).await?;
    with_migrate_lock_pool(pool, async {
        check_is_head_pool(pool, dir, name, ledger).await?;
        unapply_locked_pool(pool, dir, name, ledger).await
    })
    .await
}

/// Roll back any applied migration on either backend, even out of
/// order. Caller accepts responsibility for the resulting schema state.
///
/// # Errors
/// As [`unapply_force`].
pub async fn unapply_force_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
    name: &str,
) -> Result<Migration, MigrateError> {
    unapply_force_pool_with_ledger(pool, dir, name, LEDGER_TABLE).await
}

async fn unapply_force_pool_with_ledger(
    pool: &crate::sql::Pool,
    dir: &Path,
    name: &str,
    ledger: &str,
) -> Result<Migration, MigrateError> {
    ensure_ledger_pool_with_ledger(pool, ledger).await?;
    with_migrate_lock_pool(pool, unapply_locked_pool(pool, dir, name, ledger)).await
}

/// Compute the SQL `migrate_pool(pool, dir)` would execute, without
/// running any of it. Bi-dialect counterpart of [`migrate_dry_run`].
///
/// # Errors
/// As [`migrate_dry_run`].
pub async fn migrate_dry_run_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
) -> Result<Vec<MigrationPreview>, MigrateError> {
    migrate_dry_run_pool_with_ledger(pool, dir, LEDGER_TABLE).await
}

/// Dry-run pending migrations against a custom-named ledger.
/// Sibling of [`migrate_dry_run_pool`] (issue #146).
///
/// # Errors
/// As [`migrate_dry_run_pool`].
pub async fn migrate_dry_run_pool_with_ledger(
    pool: &crate::sql::Pool,
    dir: &Path,
    ledger: &str,
) -> Result<Vec<MigrationPreview>, MigrateError> {
    ensure_ledger_pool_with_ledger(pool, ledger).await?;
    let all = file::list_dir(dir)?;
    let applied = applied_set_pool_with_ledger(pool, ledger).await?;
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

// ---- internal helpers ----

async fn apply_one_pool(
    pool: &crate::sql::Pool,
    mig: &Migration,
    ledger: &str,
) -> Result<(), MigrateError> {
    if mig.atomic {
        apply_atomic_pool(pool, mig, ledger).await
    } else {
        apply_nonatomic_pool(pool, mig, ledger).await
    }
}

async fn unapply_all_in_order_pool(
    pool: &crate::sql::Pool,
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
        unapply_locked_pool(pool, dir, &mig.name, ledger).await?;
        touched.push(mig);
    }
    Ok(touched)
}

/// `unapply_pool`'s body without acquiring the migrate lock — used
/// by `migrate_to_pool` and `downgrade_pool` which already hold it.
async fn unapply_locked_pool(
    pool: &crate::sql::Pool,
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
        None => SchemaSnapshot {
            tables: vec![],
            m2m_tables: vec![],
            indexes: vec![],
            checks: vec![],
            excludes: vec![],
        },
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
        unapply_atomic_pool(pool, &target, &inverted, &prev_snapshot, ledger).await?;
    } else {
        unapply_nonatomic_pool(pool, &target, &inverted, &prev_snapshot, ledger).await?;
    }

    Ok(target)
}

async fn check_is_head_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
    name: &str,
    ledger: &str,
) -> Result<(), MigrateError> {
    let applied = applied_set_pool_with_ledger(pool, ledger).await?;
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
             Use `downgrade_pool(pool, dir, n)` / `migrate_to_pool(pool, dir, target)` for \
             ordered rollback, or `unapply_force_pool` to bypass.",
        ))),
        None => Ok(()),
    }
}

async fn unapply_atomic_pool(
    pool: &crate::sql::Pool,
    target: &Migration,
    inverted: &[Operation],
    snapshot: &SchemaSnapshot,
    ledger: &str,
) -> Result<(), MigrateError> {
    tracing::info!(migration = %target.name, "unapplying (atomic, _pool)");
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in inverted {
                match op {
                    Operation::Schema(change) => {
                        let batch = super::diff::render_changes_split_with_dialect(
                            std::slice::from_ref(change),
                            snapshot,
                            pool.dialect(),
                        )
                        .map_err(MigrateError::Validation)?;
                        for stmt in batch.immediate {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => {
                        // #347 — see `invoke_migration_callback` doc.
                        invoke_migration_callback(c, pool.clone().into()).await?;
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
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            // MySQL: implicit-commit on every DDL statement defeats the
            // atomic-rollback wrapper here too. A failure mid-unapply
            // leaves the schema half-reverted. See `apply_atomic_pool`'s
            // MySQL arm for the full caveat doc. Tracked in #559.
            tracing::warn!(
                migration = %target.name,
                "MySQL silently auto-commits on every DDL statement; the atomic-unapply \
                 wrapper only protects RunSQL/RunPython between DDL ops. A failure mid-unapply \
                 leaves the schema half-reverted and requires manual recovery."
            );
            let mut tx = my.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in inverted {
                match op {
                    Operation::Schema(change) => {
                        let batch = super::diff::render_changes_split_with_dialect(
                            std::slice::from_ref(change),
                            snapshot,
                            pool.dialect(),
                        )
                        .map_err(MigrateError::Validation)?;
                        for stmt in batch.immediate {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => {
                        // #347 — see `invoke_migration_callback` doc.
                        invoke_migration_callback(c, pool.clone().into()).await?;
                    }
                }
            }
            for stmt in deferred_fks {
                sqlx::query(&stmt).execute(&mut *tx).await?;
            }
            sqlx::query(&format!("DELETE FROM {ledger} WHERE name = ?"))
                .bind(&target.name)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut tx = sq.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in inverted {
                match op {
                    Operation::Schema(change) => {
                        let batch = super::diff::render_changes_split_with_dialect(
                            std::slice::from_ref(change),
                            snapshot,
                            pool.dialect(),
                        )
                        .map_err(MigrateError::Validation)?;
                        for stmt in batch.immediate {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => {
                        // #347 — see `invoke_migration_callback` doc.
                        invoke_migration_callback(c, pool.clone().into()).await?;
                    }
                }
            }
            for stmt in deferred_fks {
                sqlx::query(&stmt).execute(&mut *tx).await?;
            }
            sqlx::query(&format!("DELETE FROM {ledger} WHERE name = ?"))
                .bind(&target.name)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
    }
    Ok(())
}

/// Apply pending migrations from an in-memory `&[(name, json)]` slice
/// against either backend. Bi-dialect counterpart of [`migrate_embedded`].
///
/// Built for single-binary deployments where shipping a `migrations/`
/// folder alongside the binary is awkward (Docker images, scratch
/// containers, embedded systems). Pair with the
/// [`embed_migrations!`](crate::embed_migrations) proc-macro, which
/// scans a directory at compile time and emits the slice via
/// `include_str!` per file.
///
/// Each entry's first item must equal the migration's `name` field
/// — a divergence would mean the slice was hand-built incorrectly.
///
/// # Errors
/// As [`migrate_embedded`], plus [`MigrateError::Validation`] when an
/// entry key doesn't match the migration's own `name` field.
pub async fn migrate_embedded_pool(
    pool: &crate::sql::Pool,
    embedded: &[(&str, &str)],
) -> Result<Vec<Migration>, MigrateError> {
    migrate_embedded_pool_with_ledger(pool, embedded, LEDGER_TABLE).await
}

async fn migrate_embedded_pool_with_ledger(
    pool: &crate::sql::Pool,
    embedded: &[(&str, &str)],
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    ensure_ledger_pool_with_ledger(pool, ledger).await?;
    with_migrate_lock_pool(pool, async {
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

        let applied = applied_set_pool_with_ledger(pool, ledger).await?;
        let pending: Vec<Migration> = all
            .into_iter()
            .filter(|m| !applied.contains(&m.name))
            .collect();

        let mut newly = Vec::with_capacity(pending.len());
        for mig in pending {
            apply_one_pool(pool, &mig, ledger).await?;
            newly.push(mig);
        }
        Ok(newly)
    })
    .await
}

async fn unapply_nonatomic_pool(
    pool: &crate::sql::Pool,
    target: &Migration,
    inverted: &[Operation],
    snapshot: &SchemaSnapshot,
    ledger: &str,
) -> Result<(), MigrateError> {
    tracing::info!(migration = %target.name, "unapplying (non-atomic, _pool)");
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in inverted {
        match op {
            Operation::Schema(change) => {
                let batch = super::diff::render_changes_split_with_dialect(
                    std::slice::from_ref(change),
                    snapshot,
                    pool.dialect(),
                )
                .map_err(MigrateError::Validation)?;
                for stmt in batch.immediate {
                    crate::sql::raw_execute_pool(pool, &stmt, ::std::vec::Vec::new()).await?;
                }
                deferred_fks.extend(batch.deferred_fks);
            }
            Operation::Data(d) => {
                crate::sql::raw_execute_pool(pool, &d.sql, ::std::vec::Vec::new()).await?;
            }
            Operation::Callback(c) => {
                // #347 — non-tx context; pass the pool directly.
                invoke_migration_callback(c, pool.clone()).await?;
            }
        }
    }
    for stmt in deferred_fks {
        crate::sql::raw_execute_pool(pool, &stmt, ::std::vec::Vec::new()).await?;
    }
    let placeholder = pool.dialect().placeholder(1);
    let delete_sql = format!("DELETE FROM {ledger} WHERE name = {placeholder}");
    crate::sql::raw_execute_pool(
        pool,
        &delete_sql,
        ::std::vec![crate::core::SqlValue::String(target.name.clone())],
    )
    .await?;
    Ok(())
}
