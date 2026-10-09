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
//!   transaction by default, so partial progress across files is
//!   recoverable.

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

use super::file::{self, Migration, Operation};
use super::invert::invert;
use super::progress::{emit, MigrationEvent, MigrationObserver, Outcome};
use super::snapshot::SchemaSnapshot;
use super::{ddl, MigrateError};

/// Default bookkeeping-table name — stores one row per applied
/// migration. Double-underscored to avoid colliding with user
/// tables. Override per-app via `Builder::ledger`.
pub const LEDGER_TABLE: &str = "__rustango_migrations__";

/// Ledger for the framework's own ("system app") migrations — the
/// generated `system/migrations/` chain that stands up the `rustango_*`
/// tables. Kept separate from [`LEDGER_TABLE`] so the framework's chain and
/// the project's never collide or renumber each other.
pub const SYSTEM_LEDGER_TABLE: &str = "__rustango_system_migrations__";

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
        migrate_with_ledger(pool, dir, self.ledger, None).await
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
#[cfg(feature = "postgres")]
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

/// The subset of [`registered_models`] the inventory-walk bootstrap
/// (`apply_all*` / `drop_all*`) should touch: framework-managed tables
/// only. A model marked `managed = false` means the framework
/// neither creates nor drops the table — the snapshot / migration path
/// already filters these (see `snapshot.rs`), and the bootstrap walk
/// must match. Otherwise a `managed = false` model (e.g.
/// `rustango_translations`, which owns its schema via its own
/// `ensure_table`) would get a second, schema-mismatched `CREATE TABLE`
/// emitted from its field metadata here.
fn bootstrap_models() -> Vec<&'static ModelSchema> {
    registered_models()
        .into_iter()
        .filter(|m| m.managed)
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
    // `signals` is optional, so the pre/post_migrate emissions are gated
    // (#1208) — they were unconditional while the module is not.
    #[cfg(feature = "signals")]
    use crate::signals::migrate::{
        send_post_migrate, send_pre_migrate, PostMigrateContext, PreMigrateContext,
    };
    #[cfg(feature = "signals")]
    send_pre_migrate(PreMigrateContext {
        source: "apply_all",
    })
    .await;
    let models = bootstrap_models();

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
    #[cfg(feature = "signals")]
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
    for model in bootstrap_models() {
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
    for model in bootstrap_models() {
        ddl::check_on_delete(pool.dialect(), model).map_err(MigrateError::Validation)?;
    }
    #[cfg(feature = "signals")]
    use crate::signals::migrate::{
        send_post_migrate, send_pre_migrate, PostMigrateContext, PreMigrateContext,
    };
    #[cfg(feature = "signals")]
    send_pre_migrate(PreMigrateContext {
        source: "apply_all_pool",
    })
    .await;
    let dialect = pool.dialect();
    let models = bootstrap_models();
    if let Some(sql) = ddl::ci_text_extension_sql(dialect, &models) {
        crate::sql::raw_execute_pool(pool, sql, ::std::vec::Vec::new()).await?;
    }
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
    // `db_table_comment` — same shape as column-level:
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
    #[cfg(feature = "signals")]
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
    let models = bootstrap_models();
    // Phase 1 — drop FK constraints, so table drop order can't matter.
    // The mirror of `apply_all`'s two-phase create. Without it MySQL, which
    // enforces FKs and has no `DROP TABLE … CASCADE`, fails as soon as a
    // parent is dropped before its child (#1277). Best-effort: a constraint
    // may already be absent, and only PG can say `IF EXISTS` here.
    for model in &models {
        for sql in ddl::drop_constraints_sql_with_dialect(dialect, model) {
            let _ = crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await;
        }
    }
    // Phase 2 — drop the tables themselves.
    for model in &models {
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

/// [`migrate`], reporting each migration to `observer` as it starts and
/// finishes.
///
/// The PG-typed sibling of [`migrate_pool_with_progress`]. Kept separate
/// rather than routed through it because this path is not the same code:
/// it fires the `pre_migrate` / `post_migrate` signals and uses the
/// PG-typed ledger helpers.
///
/// **The observer is called with the migrate lock held**, so it must not
/// block; see [the module docs](super::progress#observers-must-not-block).
///
/// # Errors
/// As [`migrate`].
#[cfg(feature = "postgres")]
pub async fn migrate_with_progress(
    pool: &PgPool,
    dir: &Path,
    observer: &dyn MigrationObserver,
) -> Result<Vec<Migration>, MigrateError> {
    migrate_with_ledger(pool, dir, LEDGER_TABLE, Some(observer)).await
}

#[cfg(feature = "postgres")]
async fn migrate_with_ledger(
    pool: &PgPool,
    dir: &Path,
    ledger: &str,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Vec<Migration>, MigrateError> {
    with_migrate_signals(
        async {
            ensure_ledger_for(pool, ledger).await?;
            with_migrate_lock(pool, migrate_with_ledger_body(pool, dir, ledger, observer)).await
        },
        |newly| newly.iter().map(|m| m.name.clone()).collect(),
    )
    .await
}

/// [`migrate_with_progress`] for a caller that holds the migrate lock. Fires
/// no signals: they would run under the lock, so [`Signals::Fire`] does.
#[cfg(feature = "postgres")]
#[cfg_attr(not(any(feature = "manage", feature = "tenancy")), allow(dead_code))]
pub(crate) async fn migrate_locked(
    _: LockHeld,
    pool: &PgPool,
    dir: &Path,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Vec<Migration>, MigrateError> {
    let enum_pool = crate::sql::Pool::Postgres(pool.clone());
    create_ledger_locked(&enum_pool, LEDGER_TABLE).await?;
    migrate_with_ledger_body(pool, dir, LEDGER_TABLE, observer).await
}

/// Whether a run fires `pre_migrate` / `post_migrate`. They fire outside
/// the migrate lock, so a handler may migrate again.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signals {
    /// The PG `migrate` runner's contract.
    #[cfg_attr(
        not(all(feature = "postgres", any(feature = "manage", feature = "tenancy"))),
        allow(dead_code)
    )]
    Fire,
    Skip,
}

/// `pre_migrate` / `post_migrate` around `run`; `applied` names what it applied.
pub(crate) async fn with_migrate_signals<T>(
    run: impl std::future::Future<Output = Result<T, MigrateError>>,
    applied: impl FnOnce(&T) -> Vec<String>,
) -> Result<T, MigrateError> {
    #[cfg(feature = "signals")]
    use crate::signals::migrate::{
        send_post_migrate, send_pre_migrate, PostMigrateContext, PreMigrateContext,
    };
    #[cfg(feature = "signals")]
    send_pre_migrate(PreMigrateContext { source: "migrate" }).await;
    let out = run.await?;
    // #411 — post_migrate fires once after the file-based migrate
    // session completes. `applied` lists newly-applied migration
    // names (empty when everything was already applied).
    #[cfg(feature = "signals")]
    send_post_migrate(PostMigrateContext {
        source: "migrate",
        applied: applied(&out),
    })
    .await;
    #[cfg(not(feature = "signals"))]
    let _ = applied;
    Ok(out)
}

/// The legacy PG run; the caller holds the migrate lock.
#[cfg(feature = "postgres")]
async fn migrate_with_ledger_body(
    pool: &PgPool,
    dir: &Path,
    ledger: &str,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Vec<Migration>, MigrateError> {
    let all = file::list_dir(dir)?;
    let applied = applied_set_for(pool, ledger).await?;
    let pending = pending_migrations(&all, &applied);

    let total = pending.len();
    emit(observer, || MigrationEvent::Planned { total });

    let mut newly = Vec::with_capacity(total);
    for (i, mig) in pending.into_iter().enumerate() {
        let index = i + 1;
        emit(observer, || MigrationEvent::Started {
            name: mig.name.clone(),
            index,
            total,
        });
        let began = std::time::Instant::now();

        let step = reconcile_and_apply_pg(pool, &mig, &applied, ledger).await;

        let outcome = match step {
            Ok(outcome) => outcome,
            Err(e) => {
                emit(observer, || MigrationEvent::Failed {
                    name: mig.name.clone(),
                    index,
                    total,
                    error: e.to_string(),
                });
                return Err(e);
            }
        };

        emit(observer, || MigrationEvent::Finished {
            name: mig.name.clone(),
            index,
            total,
            outcome,
            elapsed: began.elapsed(),
        });
        newly.push(mig);
    }
    Ok(newly)
}

/// [`reconcile_and_apply`] for the legacy PgPool runner, which applies
/// through [`apply_one`]. `fake_initial` stays off: only the system chain
/// opts into table-existence faking.
#[cfg(feature = "postgres")]
async fn reconcile_and_apply_pg(
    pool: &PgPool,
    mig: &Migration,
    applied: &HashSet<String>,
    ledger: &str,
) -> Result<Outcome, MigrateError> {
    let enum_pool = crate::sql::Pool::Postgres(pool.clone());
    Ok(match reconcile(&enum_pool, mig, applied, false).await? {
        ReconcileAction::Fake => {
            fake_apply_pool(&enum_pool, mig, ledger).await?;
            Outcome::Faked
        }
        // Unreachable with `fake_initial` off; a plain run for totality.
        ReconcileAction::Run | ReconcileAction::RunPartial(_) => {
            apply_one(pool, mig, ledger).await?;
            Outcome::Ran
        }
        ReconcileAction::RunOutside(existing) => {
            apply_one(pool, &outside_tables(mig, &existing), ledger).await?;
            Outcome::RanPartial { skipped: existing }
        }
    })
}

/// What `migrate <target>` applies going forward: the pending files after
/// `head` up to `target`, squashes included so they reconcile (#2243).
fn forward_to(
    all: &[Migration],
    applied: &HashSet<String>,
    head: Option<&str>,
    target: &str,
) -> Vec<Migration> {
    pending_migrations(all, applied)
        .into_iter()
        .filter(|m| head.is_none_or(|h| m.name.as_str() > h) && m.name.as_str() <= target)
        .collect()
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
    hold_migrate_lock(&crate::sql::Pool::Postgres(pool.clone()), body).await
}

/// The creation schema of `pool`'s sessions, for FK targets (#1718).
#[cfg(feature = "postgres")]
async fn pg_creation_schema(pool: &PgPool) -> Result<Option<String>, MigrateError> {
    let pool = crate::sql::Pool::Postgres(pool.clone());
    Ok(super::ensure::creation_schema(&pool).await?)
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
    let dialect = Postgres;
    let mut tx = pool.begin().await?;
    // Postgres returns `pg_advisory_xact_lock(...)`. SQLite returns
    // `None` (its `BEGIN` already gates concurrent CREATE TABLE).
    if let Some(xact_lock_sql) = dialect.acquire_xact_lock_sql() {
        // The pool runners' key, so both bootstraps serialize (#1844).
        sqlx::query(&xact_lock_sql)
            .bind(MIGRATE_LOCK_KEY)
            .execute(&mut *tx)
            .await?;
    }
    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {ledger} (\
         name TEXT PRIMARY KEY, \
         applied_at {})",
        dialect.timestamp_now_column()
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
///
/// FK targets render unqualified here; on PostgreSQL the applied DDL
/// pins them to the session's schema (#1718).
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
    let pending = all.iter().filter(|m| !applied.contains(&m.name));
    let mut out = Vec::new();
    for mig in pending {
        let before = prev_snapshot(&all, mig, dir)?;
        out.push(preview_migration(
            mig,
            &before,
            &crate::sql::Postgres,
            ledger,
        )?);
    }
    Ok(out)
}

/// `sqlmigrate <name>` — compute the SQL the named
/// migration would emit when applied, without touching the database.
/// Pure file I/O + render — no ledger read required. FK targets stay
/// unqualified; PostgreSQL applies pin them to the session's schema (#1718).
///
/// Issue #345. Use from `manage sqlmigrate <name>`.
///
/// # Errors
/// - [`MigrateError::Validation`] when `name` is not present in `dir`.
/// - Any IO / parse error from [`file::list_dir`].
pub fn sqlmigrate_one(
    dir: &Path,
    name: &str,
    dialect: &dyn crate::sql::Dialect,
) -> Result<MigrationPreview, MigrateError> {
    let all = file::list_dir(dir)?;
    let mig = all.iter().find(|m| m.name == name).ok_or_else(|| {
        MigrateError::Validation(format!("migration `{name}` not found in {}", dir.display()))
    })?;
    let before = prev_snapshot(&all, mig, dir)?;
    preview_migration(mig, &before, dialect, LEDGER_TABLE)
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

/// The atomic runners' callback arm. `file::parse` already refuses this
/// shape; running it would hang on the tx's own locks (#1626).
fn callback_in_atomic(mig: &str, op: &crate::migrate::file::CallbackOp) -> MigrateError {
    MigrateError::Validation(format!(
        "{mig}: callback `{}` needs `\"atomic\": false` (#1626)",
        op.name,
    ))
}

/// The schema before `mig`: its `prev` file's snapshot, or empty.
fn prev_snapshot(
    all: &[Migration],
    mig: &Migration,
    dir: &Path,
) -> Result<SchemaSnapshot, MigrateError> {
    let Some(prev_name) = &mig.prev else {
        return Ok(SchemaSnapshot::default());
    };
    all.iter()
        .find(|m| &m.name == prev_name)
        .map(|m| m.snapshot.clone())
        .ok_or_else(|| {
            MigrateError::Validation(format!(
                "migration `{}` declares prev=`{prev_name}` but that file is missing in {}",
                mig.name,
                dir.display()
            ))
        })
}

/// Build a [`MigrationPreview`] for a single migration. Pure — no DB
/// access. Renders as the `_pool` runners do for `dialect`; `before` is
/// the schema `mig` starts from, for the FKs a MySQL column drop removes.
fn preview_migration(
    mig: &Migration,
    before: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
    ledger: &str,
) -> Result<MigrationPreview, MigrateError> {
    let mut statements = Vec::new();
    let mut deferred_fks: Vec<String> = Vec::new();
    let mut live = LiveFks::new(before);
    if mig.atomic {
        statements.push("BEGIN".to_string());
    }
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let deferred = preview_schema_op(
                    change,
                    &mig.forward,
                    &mig.snapshot,
                    before,
                    dialect,
                    &mut live,
                    &mut statements,
                )?;
                deferred_fks.extend(deferred);
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
    // The preview shows the column list the runners actually write,
    // `applied_at` included — a plan that omits a column the apply then
    // writes is a plan of a different statement.
    statements.push(format!(
        "INSERT INTO {ledger} (name, applied_at) VALUES ('{}', <now>)",
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

/// Render `change`, one of `ops`, as a preview: push its statements to
/// `statements` and return its deferred FKs. `before` is the schema `ops`
/// start from, for the FKs a MySQL column drop removes.
fn preview_schema_op(
    change: &super::SchemaChange,
    ops: &[Operation],
    after: &SchemaSnapshot,
    before: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
    live: &mut LiveFks,
    statements: &mut Vec<String>,
) -> Result<Vec<String>, MigrateError> {
    let step = render_step(change, ops, after, dialect, None)?;
    // The runner looks these names up live; this is the name it has then.
    if let (Some(fk), Some(_)) = (&step.drop_fks, dialect.foreign_key_names_sql()) {
        if let Some(name) = live.take(&fk.table, &fk.column) {
            statements.extend(dialect.drop_foreign_key_sql(&fk.table, &name));
        }
    }
    if let Some((table, column)) = &step.readded_now {
        live.added(table, column);
    }
    // Apply drops the name it finds in the catalog; this is the usual one.
    if let (Some(u), Some(_)) = (&step.drop_unique, dialect.unique_index_names_sql()) {
        let name = ddl::unique_constraint_name(&u.table, &u.column);
        statements.extend(dialect.drop_unique_index_sql(&u.table, &name));
    }
    // An index's FK, as apply finds it: by the index's first column, when no
    // other index (declared, UNIQUE or the PK) starts with it at this op.
    let earlier = ops
        .iter()
        .position(|op| matches!(op, Operation::Schema(c) if std::ptr::eq(c, change)))
        .map_or(&[][..], |i| &ops[..i]);
    let dropped = step
        .index_fks
        .as_ref()
        .and_then(|ix| Some((ix, before.indexes.iter().find(|i| i.name == ix.index)?)));
    let (mut drops, mut readd) = dropped
        .and_then(|(ix, index)| {
            let column = index.columns.first()?;
            let field = before.table(&ix.table).and_then(|t| t.field(column));
            let served = field.is_some_and(|f| f.unique || f.primary_key)
                || leading_indexes_at(before, earlier, &ix.table, column)
                    .iter()
                    .any(|name| *name != ix.index);
            if served {
                return None;
            }
            // An earlier op of the migration already dropped a live one.
            let names: Vec<String> = live
                .get(&ix.table, column)
                .filter(|_| !fk_deferred_earlier(&ix.table, column, earlier))
                .into_iter()
                .collect();
            let (drops, readd) = ix.plan(column, &names, dialect);
            if readd.is_some() {
                live.added(&ix.table, column);
            } else if !drops.is_empty() {
                live.take(&ix.table, column);
            }
            Some((drops, readd.into_iter().collect::<Vec<_>>()))
        })
        .unwrap_or_default();
    // The composite FKs only this index serves, as apply finds them (#2326).
    if let Some((ix, index)) = dropped {
        let covers = |cols: &[String], fk: &[String]| cols.starts_with(fk);
        let names: Vec<String> = before
            .table(&ix.table)
            .map_or(&[][..], |t| t.composite_fks.as_slice())
            .iter()
            .filter(|cf| covers(&index.columns, &cf.from))
            .filter(|cf| {
                !indexes_at(before, earlier, &ix.table)
                    .iter()
                    .any(|(n, cols)| *n != ix.index && covers(cols, &cf.from))
            })
            .map(|cf| cf.name.clone())
            .collect();
        let (d, r) = ix.plan_composites(&names, dialect);
        drops.extend(d);
        readd.extend(r);
    }
    statements.extend(drops);
    statements.extend(step.batch.immediate);
    statements.extend(readd);
    if let Some(rebuild) = &step.batch.rebuild {
        statements.extend(rebuild.statements(dialect));
        statements.push(format!(
            "-- re-create the indexes and triggers of {}",
            rebuild.table()
        ));
    }
    live.follow(change);
    Ok(step.batch.deferred_fks)
}

/// The column FKs live at each op of a preview, by table and column, with
/// the name each has; apply finds them in the catalog instead.
struct LiveFks(std::collections::HashMap<(String, String), String>);

impl LiveFks {
    fn new(before: &SchemaSnapshot) -> Self {
        let fields = before.tables.iter().flat_map(|t| {
            t.fields
                .iter()
                .filter(|f| f.fk.is_some())
                .map(|f| (t.name.clone(), f.column.clone()))
        });
        let junctions = before
            .m2m_tables
            .iter()
            .flat_map(|m| [&m.src_col, &m.dst_col].map(|c| (m.through.clone(), c.clone())));
        Self(
            fields
                .chain(junctions)
                .map(|(t, c)| {
                    let name = ddl::fk_constraint_name(&t, &c);
                    ((t, c), name)
                })
                .collect(),
        )
    }

    fn get(&self, table: &str, column: &str) -> Option<String> {
        self.0.get(&(table.to_owned(), column.to_owned())).cloned()
    }

    /// The live FK on `table.column`, which the op drops.
    fn take(&mut self, table: &str, column: &str) -> Option<String> {
        self.0.remove(&(table.to_owned(), column.to_owned()))
    }

    /// An FK the op adds on `table.column`, under the names then.
    fn added(&mut self, table: &str, column: &str) {
        let name = ddl::fk_constraint_name(table, column);
        self.0.insert((table.to_owned(), column.to_owned()), name);
    }

    /// Its FKs keep their names through renames, and go with their column.
    fn follow(&mut self, change: &super::SchemaChange) {
        use super::SchemaChange as SC;
        let rekey =
            |map: &mut std::collections::HashMap<_, _>,
             f: &dyn Fn(&(String, String)) -> Option<(String, String)>| {
                let moved: Vec<_> = map
                    .keys()
                    .filter_map(|k| f(k).map(|n| (k.clone(), n)))
                    .collect();
                for (old, new) in moved {
                    if let Some(name) = map.remove(&old) {
                        map.insert(new, name);
                    }
                }
            };
        match change {
            SC::RenameTable { old_name, new_name } => rekey(&mut self.0, &|(t, c)| {
                (t == old_name).then(|| (new_name.clone(), c.clone()))
            }),
            SC::RenameColumn {
                table,
                old_column,
                new_column,
            } => rekey(&mut self.0, &|(t, c)| {
                (t == table && c == old_column).then(|| (t.clone(), new_column.clone()))
            }),
            SC::DropColumn { table, column } => {
                self.take(table, column);
            }
            SC::DropTable(t) | SC::DropM2MTable { through: t } => {
                self.0.retain(|(table, _), _| table != t);
            }
            _ => {}
        }
    }
}

/// The DDL that moves `before` to `after` by `changes` on `dialect`, as
/// `sqlmigrate` prints it: a MySQL column drop drops its FK first (#2026).
///
/// # Errors
/// A change the dialect cannot render.
pub fn render_changes_between(
    changes: &[super::SchemaChange],
    before: &SchemaSnapshot,
    after: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Result<Vec<String>, MigrateError> {
    let ops: Vec<Operation> = changes.iter().cloned().map(Operation::Schema).collect();
    let (mut statements, mut deferred) = (Vec::new(), Vec::new());
    let mut live = LiveFks::new(before);
    for op in &ops {
        if let Operation::Schema(change) = op {
            deferred.extend(preview_schema_op(
                change,
                &ops,
                after,
                before,
                dialect,
                &mut live,
                &mut statements,
            )?);
        }
    }
    statements.extend(deferred);
    Ok(statements)
}

#[cfg(feature = "postgres")]
async fn apply_atomic(pool: &PgPool, mig: &Migration, ledger: &str) -> Result<(), MigrateError> {
    tracing::info!(migration = %mig.name, "applying (atomic)");
    let schema = pg_creation_schema(pool).await?;
    let mut tx = pool.begin().await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let step = render_step(
                    change,
                    &mig.forward,
                    &mig.snapshot,
                    &crate::sql::Postgres,
                    schema.as_deref(),
                )?;
                for stmt in pg_statements(&mut tx, &step).await? {
                    sqlx::query(&stmt).execute(&mut *tx).await?;
                }
                deferred_fks.extend(step.batch.deferred_fks);
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(&mut *tx).await?;
            }
            Operation::Callback(c) => return Err(callback_in_atomic(&mig.name, c)),
        }
    }
    for stmt in deferred_fks {
        sqlx::query(&stmt).execute(&mut *tx).await?;
    }
    sqlx::query(&ledger_insert_sql(&crate::sql::Postgres, ledger))
        .bind(&mig.name)
        .bind(chrono::Utc::now())
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
                for mig in forward_to(&all, &applied, None, target) {
                    reconcile_and_apply_pg(pool, &mig, &applied, ledger).await?;
                    touched.push(mig);
                }
            }
            Some(h) => {
                use std::cmp::Ordering;
                match target.cmp(h.as_str()) {
                    Ordering::Equal => {}
                    Ordering::Greater => {
                        for mig in forward_to(&all, &applied, Some(&h), target) {
                            reconcile_and_apply_pg(pool, &mig, &applied, ledger).await?;
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

    let prev_snapshot = prev_snapshot(&all, &target, dir)?;

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
    let schema = pg_creation_schema(pool).await?;
    let mut tx = pool.begin().await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in inverted {
        match op {
            Operation::Schema(change) => {
                let step = render_step(
                    change,
                    inverted,
                    snapshot,
                    &crate::sql::Postgres,
                    schema.as_deref(),
                )?;
                for stmt in pg_statements(&mut tx, &step).await? {
                    sqlx::query(&stmt).execute(&mut *tx).await?;
                }
                deferred_fks.extend(step.batch.deferred_fks);
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql).execute(&mut *tx).await?;
            }
            Operation::Callback(c) => return Err(callback_in_atomic(&target.name, c)),
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
    let schema = pg_creation_schema(pool).await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in inverted {
        match op {
            Operation::Schema(change) => {
                let step = render_step(
                    change,
                    inverted,
                    snapshot,
                    &crate::sql::Postgres,
                    schema.as_deref(),
                )?;
                let mut conn = pool.acquire().await?;
                for stmt in pg_statements(&mut conn, &step).await? {
                    sqlx::query(&stmt).execute(&mut *conn).await?;
                }
                deferred_fks.extend(step.batch.deferred_fks);
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
    let schema = pg_creation_schema(pool).await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let step = render_step(
                    change,
                    &mig.forward,
                    &mig.snapshot,
                    &crate::sql::Postgres,
                    schema.as_deref(),
                )?;
                let mut conn = pool.acquire().await?;
                for stmt in pg_statements(&mut conn, &step).await? {
                    sqlx::query(&stmt).execute(&mut *conn).await?;
                }
                deferred_fks.extend(step.batch.deferred_fks);
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
    sqlx::query(&ledger_insert_sql(&crate::sql::Postgres, ledger))
        .bind(&mig.name)
        .bind(chrono::Utc::now())
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
    with_migrate_lock_pool(pool, ledger, async { Ok(()) }).await
}

/// The ledger `CREATE TABLE IF NOT EXISTS`. Only under the migrate lock:
/// concurrent PG creators collide on `pg_type` (23505) (#1844).
async fn create_ledger_locked(pool: &crate::sql::Pool, ledger: &str) -> Result<(), MigrateError> {
    // Was a three-arm match on the dialect name with a hand-written
    // type + DEFAULT each, whose SQLite arm carried a copy of #1464's
    // canonical `strftime`. `Dialect` answers both halves now, so the
    // unreachable "unrecognized dialect" error goes too.
    let timestamp_col = pool.dialect().timestamp_now_column();
    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {ledger} (\
         name VARCHAR(255) PRIMARY KEY, \
         applied_at {timestamp_col})"
    );
    crate::sql::raw_execute_pool(pool, &create_sql, ::std::vec::Vec::new()).await?;
    Ok(())
}

/// `ledger`'s names, creating it first: on PG a read before a tenant schema
/// has its ledger finds `public`'s, and the cached plan keeps it (#2143).
pub(crate) async fn ledger_names(
    _: LockHeld,
    pool: &crate::sql::Pool,
    ledger: &str,
) -> Result<HashSet<String>, MigrateError> {
    create_ledger_locked(pool, ledger).await?;
    applied_set_pool_with_ledger(pool, ledger).await
}

/// The statement that records a migration as applied — one column list
/// for the five runners that write it.
///
/// `applied_at` is bound, not defaulted (#1464): a defaulted write on
/// an upgraded SQLite file puts the legacy spelling back into a column
/// `migrate`'s own sweep has just normalised. Nothing compares the
/// column today, so this is housekeeping, not a live defect.
fn ledger_insert_sql(dialect: &dyn crate::sql::Dialect, ledger: &str) -> String {
    let (p1, p2) = (dialect.placeholder(1), dialect.placeholder(2));
    format!("INSERT INTO {ledger} (name, applied_at) VALUES ({p1}, {p2})")
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
/// Concurrent runs are serialized by the migrate lock (`pg_advisory_lock`
/// / `GET_LOCK`), ledger bootstrap included; SQLite relies on its file lock.
///
/// # Errors
/// As [`migrate`].
pub async fn migrate_pool(
    pool: &crate::sql::Pool,
    dir: &Path,
) -> Result<Vec<Migration>, MigrateError> {
    migrate_pool_with_ledger(pool, dir, LEDGER_TABLE).await
}

/// [`migrate_pool`], reporting each migration to `observer` as it starts
/// and finishes.
///
/// The observer sees the pending count up front, then a
/// [`Started`](MigrationEvent::Started) and a
/// [`Finished`](MigrationEvent::Finished) per migration — carrying which
/// of the three apply paths it took and how long it took — or a
/// [`Failed`](MigrationEvent::Failed) naming the migration that died.
///
/// **The observer is called with the migrate lock held**, so it must not
/// block; see [the module docs](super::progress#observers-must-not-block).
///
/// # Errors
/// As [`migrate_pool`]. The observer never changes the outcome: a run
/// with an observer applies exactly what the same run without one would.
pub async fn migrate_pool_with_progress(
    pool: &crate::sql::Pool,
    dir: &Path,
    observer: &dyn MigrationObserver,
) -> Result<Vec<Migration>, MigrateError> {
    migrate_pool_with_ledger_opts(
        pool,
        dir,
        LEDGER_TABLE,
        false,
        ChainOrigin::OnDisk,
        Some(observer),
    )
    .await
}

/// [`migrate_pool_with_ledger_fake_initial`] with progress reporting —
/// the framework's own system-migration path, which is where
/// [`Outcome::Faked`] actually shows up.
///
/// # Errors
/// As [`migrate_pool_with_ledger_fake_initial`].
pub async fn migrate_pool_with_ledger_fake_initial_with_progress(
    pool: &crate::sql::Pool,
    dir: &Path,
    ledger: &str,
    observer: &dyn MigrationObserver,
) -> Result<Vec<Migration>, MigrateError> {
    migrate_pool_with_ledger_opts(pool, dir, ledger, true, ChainOrigin::OnDisk, Some(observer))
        .await
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
    migrate_pool_with_ledger_opts(pool, dir, ledger, false, ChainOrigin::OnDisk, None).await
}

/// Like [`migrate_pool_with_ledger`], but with **guarded fake-initial**
/// reconciliation (#1167): before applying, any pending migration that
/// is *purely* table-creation (`CreateTable` ops only) whose tables
/// **all already exist** is recorded in the ledger *without running its
/// SQL*. This is the upgrade path for a subsystem that used to build
/// its tables via the lazy `ensure_table` DDL and is now managed by a
/// system migration: on the first `migrate` after the upgrade, the
/// freshly-generated `CREATE TABLE` would otherwise collide with the
/// already-present table. Faking it records the migration as applied so
/// the chain moves on; existing data is untouched.
///
/// Scoped to the framework's own system-migration apply path (see
/// [`crate::tenancy`] provisioning) — user migrations go through the
/// plain [`migrate_pool_with_ledger`] and never auto-fake. The guard is
/// deliberately narrow: a migration with *any* non-`CreateTable`
/// operation (a column add, a data backfill, a callback) is never
/// faked, and a create-migration is faked only when **every** table it
/// creates is already present (a partial state falls through to the
/// runner, which surfaces the collision loudly rather than papering
/// over it).
///
/// # Errors
/// As [`migrate_pool_with_ledger`], plus a ledger-insert failure while
/// recording a faked migration.
pub async fn migrate_pool_with_ledger_fake_initial(
    pool: &crate::sql::Pool,
    dir: &Path,
    ledger: &str,
) -> Result<Vec<Migration>, MigrateError> {
    migrate_pool_with_ledger_opts(pool, dir, ledger, true, ChainOrigin::OnDisk, None).await
}

/// Where a system chain's files came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainOrigin {
    /// Shipped with the binary: its names match the ledger.
    OnDisk,
    /// Written from today's models into an empty dir. Its names depend on
    /// the features and version, so the ledger says nothing about it (#1988).
    Regenerated,
}

/// Apply `chain`'s files in `run_dir` (its dir, or a filtered copy of it)
/// under the system ledger.
///
/// # Errors
/// As [`migrate_pool_with_ledger_fake_initial`], plus catalog reads and
/// what a [`ChainOrigin::Regenerated`] chain could not converge.
pub(crate) async fn migrate_system_chain(
    _: LockHeld,
    pool: &crate::sql::Pool,
    chain: &super::make::SystemChain,
    run_dir: &Path,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Vec<Migration>, MigrateError> {
    let origin = chain.origin();
    create_ledger_locked(pool, SYSTEM_LEDGER_TABLE).await?;
    migrate_pool_body(pool, run_dir, SYSTEM_LEDGER_TABLE, true, origin, observer).await
}

/// [`migrate_pool_with_progress`] for a caller that holds the migrate lock.
pub(crate) async fn migrate_pool_locked(
    _: LockHeld,
    pool: &crate::sql::Pool,
    dir: &Path,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Vec<Migration>, MigrateError> {
    create_ledger_locked(pool, LEDGER_TABLE).await?;
    migrate_pool_body(
        pool,
        dir,
        LEDGER_TABLE,
        false,
        ChainOrigin::OnDisk,
        observer,
    )
    .await
}

async fn migrate_pool_with_ledger_opts(
    pool: &crate::sql::Pool,
    dir: &Path,
    ledger: &str,
    fake_initial: bool,
    origin: ChainOrigin,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Vec<Migration>, MigrateError> {
    with_migrate_lock_pool(
        pool,
        ledger,
        migrate_pool_body(pool, dir, ledger, fake_initial, origin, observer),
    )
    .await
}

/// The pool runner's apply loop; the caller holds the migrate lock.
async fn migrate_pool_body(
    pool: &crate::sql::Pool,
    dir: &Path,
    ledger: &str,
    fake_initial: bool,
    origin: ChainOrigin,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Vec<Migration>, MigrateError> {
    let all = file::list_dir(dir)?;
    let mut applied = applied_set_pool_with_ledger(pool, ledger).await?;
    if origin == ChainOrigin::Regenerated && !applied.is_empty() {
        // Match the live schema, not the names, then record the chain.
        let mut unfixed = Vec::new();
        for mig in &all {
            let failed = converge_regenerated(pool, mig).await?;
            if failed.is_empty() {
                if applied.insert(mig.name.clone()) {
                    fake_apply_pool(pool, mig, ledger).await?;
                }
            } else {
                unfixed.extend(failed);
            }
        }
        if !unfixed.is_empty() {
            return Err(MigrateError::Validation(format!(
                "the framework schema is missing objects `migrate` cannot add; \
                     add them by hand, then rerun `migrate`:\n  - {}",
                unfixed.join("\n  - ")
            )));
        }
    }
    let pending = pending_migrations(&all, &applied);

    let total = pending.len();
    emit(observer, || MigrationEvent::Planned { total });

    let mut newly = Vec::with_capacity(total);
    for (i, mig) in pending.into_iter().enumerate() {
        let index = i + 1;
        emit(observer, || MigrationEvent::Started {
            name: mig.name.clone(),
            index,
            total,
        });
        let began = std::time::Instant::now();

        // The whole apply is wrapped so a failure can be reported to
        // the observer before it propagates. `?` on its own would
        // leave a watcher looking at a migration stuck on "started"
        // forever, which is the state this exists to prevent.
        let outcome = reconcile_and_apply(pool, &mig, &applied, ledger, fake_initial).await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                emit(observer, || MigrationEvent::Failed {
                    name: mig.name.clone(),
                    index,
                    total,
                    error: e.to_string(),
                });
                return Err(e);
            }
        };

        emit(observer, || MigrationEvent::Finished {
            name: mig.name.clone(),
            index,
            total,
            outcome,
            elapsed: began.elapsed(),
        });
        newly.push(mig);
    }
    Ok(newly)
}

/// Reconcile and apply a single pending migration, reporting which of
/// the three paths it took.
///
/// Split out of the loop so the caller can time it and report a failure
/// with the migration's name attached — the runner's `?` alone loses
/// which file died.
async fn reconcile_and_apply(
    pool: &crate::sql::Pool,
    mig: &Migration,
    applied: &HashSet<String>,
    ledger: &str,
    fake_initial: bool,
) -> Result<Outcome, MigrateError> {
    match reconcile(pool, mig, applied, fake_initial).await? {
        ReconcileAction::Fake => {
            fake_apply_pool(pool, mig, ledger).await?;
            Ok(Outcome::Faked)
        }
        other => {
            let (effective, outcome) = match other {
                ReconcileAction::RunPartial(existing) => (
                    std::borrow::Cow::Owned(without_tables(mig, &existing)),
                    Outcome::RanPartial { skipped: existing },
                ),
                ReconcileAction::RunOutside(existing) => (
                    std::borrow::Cow::Owned(outside_tables(mig, &existing)),
                    Outcome::RanPartial { skipped: existing },
                ),
                _ => (std::borrow::Cow::Borrowed(mig), Outcome::Ran),
            };
            apply_one_pool(pool, &effective, ledger).await?;
            Ok(outcome)
        }
    }
}

// ------------------------------------------------------- reconciliation

/// What to do with a pending migration once it has been reconciled
/// against the database's actual state. See [`reconcile`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReconcileAction {
    /// Apply normally — run its `forward` ops, then record it.
    Run,
    /// Record it as applied (and tombstone anything it `replaces`)
    /// **without** running any DDL: the end state is already present.
    Fake,
    /// Apply it, but skip the operations for the listed tables — they
    /// already exist (created by the retired `ensure_*` DDL of an older
    /// build). Only the framework's own system chain uses this.
    RunPartial(Vec<String>),
    /// Apply only the schema ops on other tables: a squash whose tables
    /// already exist still carries changes to tables it does not create.
    RunOutside(Vec<String>),
}

/// Decide whether a pending migration should run or be reconciled.
///
/// Two independent reconciliation paths:
///
/// * **Squash** (`replaces` non-empty) — always considered, on every
///   runner. A squash collapses historical migrations into one file that
///   recreates the same end state, so on a database that already has that
///   history it must not re-run:
///   - every replaced migration present in the ledger → [`Fake`] (same-ledger)
///   - none present, but all the tables it creates already exist → [`Fake`]
///     (cross-ledger, the guarded fake-initial reconcile)
///   - a *partial* match either way → **error**, because the database is in
///     a state no automatic choice can safely resolve
///   - otherwise (fresh database) → [`Run`]
///
/// * **Plain migration** (`replaces` empty) — only when `fake_initial` is
///   set (the framework's own system-migration path, #1174): a *purely*
///   table-creating migration whose tables all already exist is faked, so a
///   subsystem that used to build its tables via the retired `ensure_table`
///   DDL upgrades cleanly. Deliberately conservative: any non-`CreateTable`
///   operation, or a partially-present table set, falls through to [`Run`]
///   so real work is never skipped and genuine collisions still surface.
///
/// [`Fake`]: ReconcileAction::Fake
/// [`Run`]: ReconcileAction::Run
async fn reconcile(
    pool: &crate::sql::Pool,
    mig: &Migration,
    applied: &HashSet<String>,
    fake_initial: bool,
) -> Result<ReconcileAction, MigrateError> {
    if !mig.replaces.is_empty() {
        let present = mig.replaces.iter().filter(|n| applied.contains(*n)).count();
        if present == mig.replaces.len() {
            tracing::info!(
                migration = %mig.name,
                replaces = ?mig.replaces,
                "reconciling squash — every replaced migration is already applied; \
                 recording it and tombstoning them without running DDL"
            );
            return Ok(ReconcileAction::Fake);
        }
        if present > 0 {
            let missing: Vec<&str> = mig
                .replaces
                .iter()
                .filter(|n| !applied.contains(*n))
                .map(String::as_str)
                .collect();
            return Err(MigrateError::Validation(format!(
                "cannot reconcile squash `{}`: it replaces {} migration(s) but only {} are \
                 recorded as applied (missing: {}). The database is in a partial state — \
                 resolve it by hand (see `migrate --fake <name>`) rather than guessing.",
                mig.name,
                mig.replaces.len(),
                present,
                missing.join(", "),
            )));
        }
        // None of the replaced set is in THIS ledger. The history may still
        // be present (a different ledger, or tables built out-of-band), so
        // fall back to comparing against the tables themselves.
        let targets = create_table_targets(mig);
        if targets.is_empty() {
            return Ok(ReconcileAction::Run);
        }
        let existing = count_existing_tables(pool, &targets).await;
        if existing == targets.len() {
            // Its changes to other tables: run them if none ran, fake if all did.
            let outside: Vec<&super::SchemaChange> = mig
                .forward
                .iter()
                .filter_map(|op| match op {
                    Operation::Schema(c) if !targets.iter().any(|t| t == c.table()) => Some(c),
                    _ => None,
                })
                .collect();
            let (present, absent) = applied_counts(pool, &outside).await?;
            if present > 0 && absent > 0 {
                return Err(MigrateError::Validation(format!(
                    "cannot reconcile squash `{}`: its tables exist and {present} of its other \
                     changes are applied, {absent} are not. The database is in a partial \
                     state — resolve it by hand (see `migrate --fake <name>`).",
                    mig.name
                )));
            }
            if absent > 0 {
                if mig
                    .forward
                    .iter()
                    .any(|op| !matches!(op, Operation::Schema(_)))
                {
                    return Err(MigrateError::Validation(format!(
                        "cannot reconcile squash `{}`: its tables exist but its other changes \
                         do not, and whether its data operations ran is unknown. Resolve it \
                         by hand (see `migrate --fake <name>`).",
                        mig.name
                    )));
                }
                tracing::info!(
                    migration = %mig.name,
                    tables = %targets.join(", "),
                    "reconciling squash — its tables already exist (cross-ledger); \
                     running only its changes to other tables"
                );
                return Ok(ReconcileAction::RunOutside(targets));
            }
            tracing::info!(
                migration = %mig.name,
                tables = %targets.join(", "),
                "reconciling squash — its tables already exist (cross-ledger \
                 fake-initial); recording it without running DDL"
            );
            return Ok(ReconcileAction::Fake);
        }
        if existing > 0 {
            return Err(MigrateError::Validation(format!(
                "cannot reconcile squash `{}`: {} of its {} tables already exist but the rest \
                 do not. The database is in a partial state — resolve it by hand (see \
                 `migrate --fake <name>`) rather than guessing.",
                mig.name,
                existing,
                targets.len(),
            )));
        }
        return Ok(ReconcileAction::Run);
    }

    // Plain migration: only the framework's own system-migration path opts
    // into table-existence reconciliation.
    if fake_initial {
        if let Some(tables) = create_only_tables(mig) {
            let existing: Vec<String> = {
                let mut present = Vec::new();
                for t in &tables {
                    if count_existing_tables(pool, std::slice::from_ref(t)).await == 1 {
                        present.push(t.clone());
                    }
                }
                present
            };
            if existing.len() == tables.len() {
                tracing::warn!(
                    migration = %mig.name,
                    tables = %tables.join(", "),
                    "fake-initial: tables already exist (pre-migration \
                     `ensure_table` era) — recorded as applied without \
                     running its CREATE TABLE; existing data left intact"
                );
                return Ok(ReconcileAction::Fake);
            }
            if !existing.is_empty() {
                // Partial: some of the framework's tables predate this chain
                // (the `ensure_table` era created them piecemeal — whichever
                // subsystems the app actually touched). Create only what's
                // missing and leave the rest alone. That is exactly the
                // `CREATE TABLE IF NOT EXISTS` semantics the retired `ensure_*`
                // calls had, so upgrading is never worse than before; refusing
                // here instead would simply break the upgrade.
                tracing::warn!(
                    migration = %mig.name,
                    existing = %existing.join(", "),
                    "fake-initial: some framework tables already exist — creating \
                     only the missing ones and leaving the existing untouched"
                );
                return Ok(ReconcileAction::RunPartial(existing));
            }
        }
    }
    Ok(ReconcileAction::Run)
}

/// A copy of `mig` with every operation that targets an already-present
/// table removed — the `RunPartial` payload.
///
/// Both the `CreateTable` and any `CreateIndex` for a skipped table are
/// dropped: the table was created by an older framework build, which created
/// its indexes too, so re-running them would collide the same way. Operations
/// for tables we *are* creating pass through untouched.
pub(crate) fn without_tables(mig: &Migration, existing: &[String]) -> Migration {
    use super::diff::SchemaChange as SC;
    let skip = |t: &String| existing.iter().any(|e| e == t);
    let forward = mig
        .forward
        .iter()
        .filter(|op| match op {
            Operation::Schema(SC::CreateTable(t)) => !skip(t),
            Operation::Schema(SC::CreateIndex { table, .. }) => !skip(table),
            Operation::Schema(SC::CreateM2MTable { through, .. }) => !skip(through),
            _ => true,
        })
        .cloned()
        .collect();
    Migration {
        forward,
        ..mig.clone()
    }
}

/// How many of `changes` the live schema shows applied and not applied;
/// a change it cannot tell counts as neither.
async fn applied_counts(
    pool: &crate::sql::Pool,
    changes: &[&super::SchemaChange],
) -> Result<(usize, usize), MigrateError> {
    use super::SchemaChange as SC;
    let schema = super::ensure::creation_schema(pool)
        .await?
        .unwrap_or_default();
    let (mut present, mut absent) = (0, 0);
    for change in changes {
        let applied = match change {
            SC::CreateTable(t) | SC::CreateM2MTable { through: t, .. } => {
                Some(table_exists_here(pool, t).await)
            }
            SC::DropTable(t) | SC::DropM2MTable { through: t } => {
                Some(!table_exists_here(pool, t).await)
            }
            SC::AddColumn { table, column } => Some(
                super::ensure::live_columns(pool, table)
                    .await?
                    .contains(column),
            ),
            SC::DropColumn { table, column } => Some(
                !super::ensure::live_columns(pool, table)
                    .await?
                    .contains(column),
            ),
            SC::CreateIndex { name, table, .. } => Some(
                !super::inspectdb::index_columns(pool, &schema, table, name)
                    .await?
                    .is_empty(),
            ),
            SC::DropIndex { name, table } => Some(
                super::inspectdb::index_columns(pool, &schema, table, name)
                    .await?
                    .is_empty(),
            ),
            _ => None,
        };
        match applied {
            Some(true) => present += 1,
            Some(false) => absent += 1,
            None => {}
        }
    }
    Ok((present, absent))
}

/// `mig` with only its schema ops on tables outside `existing`.
fn outside_tables(mig: &Migration, existing: &[String]) -> Migration {
    let forward = mig
        .forward
        .iter()
        .filter(|op| match op {
            Operation::Schema(c) => !existing.iter().any(|e| e == c.table()),
            _ => false,
        })
        .cloned()
        .collect();
    Migration {
        forward,
        ..mig.clone()
    }
}

/// The migrations in `all` that still need to be considered, given the
/// `applied` ledger set.
///
/// Beyond the obvious "not in the ledger" filter, this drops anything that an
/// **applied squash** declares it `replaces`. Once a squash is recorded, its
/// predecessors' ledger rows are tombstoned, but their *files* usually remain
/// on disk for a release or two, so older deployments can still migrate
/// forward. Without this filter those files would look pending
/// on the very next run and try to recreate tables that already exist.
fn pending_migrations(all: &[Migration], applied: &HashSet<String>) -> Vec<Migration> {
    let superseded: HashSet<&str> = all
        .iter()
        .filter(|m| applied.contains(&m.name))
        .flat_map(|m| m.replaces.iter().map(String::as_str))
        .collect();
    all.iter()
        .filter(|m| !applied.contains(&m.name) && !superseded.contains(m.name.as_str()))
        .cloned()
        .collect()
}

/// Every table this migration creates, in order. Unlike
/// [`create_only_tables`] this does **not** require the migration to be
/// purely table-creating — a squash legitimately carries other operations
/// alongside its `CreateTable`s, and it is the created tables that tell us
/// whether the end state is already present.
fn create_table_targets(mig: &Migration) -> Vec<String> {
    use super::diff::SchemaChange as SC;
    mig.forward
        .iter()
        .filter_map(|op| match op {
            Operation::Schema(SC::CreateTable(t) | SC::CreateM2MTable { through: t, .. }) => {
                Some(t.clone())
            }
            _ => None,
        })
        .collect()
}

/// The tables `mig` creates, when the migration does **nothing but** stand
/// those tables up; otherwise `None`.
///
/// "Nothing but" is judged against the real shape a generated initial
/// migration has, which is *not* only `CreateTable`: `makemigrations` emits
/// the table, then its indexes (and any M2M join table) as sibling
/// operations. A media-subsystem initial, for instance, is 4 `CreateTable` +
/// 6 `CreateIndex`. Requiring literal `CreateTable`-purity would reject every
/// real migration and silently disable fake-initial reconciliation — which is
/// exactly what it did before this was fixed.
///
/// So the accepted set is "operations that are part of creating these
/// tables":
/// * `CreateTable`
/// * `CreateIndex` / `CreateM2MTable`, but **only** when they target a table
///   this same migration creates — an index added to a *pre-existing* table
///   is real work that must not be skipped.
///
/// Everything else (a column add, an alter, a drop, a data backfill, a
/// callback) disqualifies the migration, so a genuine side effect is never
/// faked away.
fn create_only_tables(mig: &Migration) -> Option<Vec<String>> {
    use super::diff::SchemaChange as SC;
    let mut tables: Vec<String> = Vec::new();
    let mut targeted: Vec<&str> = Vec::new();
    for op in &mig.forward {
        match op {
            Operation::Schema(SC::CreateTable(t)) => tables.push(t.clone()),
            Operation::Schema(SC::CreateIndex { table, .. }) => targeted.push(table.as_str()),
            Operation::Schema(SC::CreateM2MTable { through, .. }) => tables.push(through.clone()),
            _ => return None,
        }
    }
    if tables.is_empty() {
        return None;
    }
    // Every index must belong to a table this migration itself creates.
    if targeted
        .iter()
        .any(|t| !tables.iter().any(|created| created == t))
    {
        return None;
    }
    Some(tables)
}

/// Create what `mig` creates and the database lacks: missing tables with
/// their indexes, and missing columns of tables already there. One object
/// at a time, so one failure does not block the rest; returns the failures.
async fn converge_regenerated(
    pool: &crate::sql::Pool,
    mig: &Migration,
) -> Result<Vec<String>, MigrateError> {
    use super::diff::SchemaChange as SC;
    let mut groups: Vec<Vec<SC>> = Vec::new();
    for op in &mig.forward {
        let Operation::Schema(change) = op else {
            continue;
        };
        let table = match change {
            SC::CreateTable(t) => t,
            SC::CreateM2MTable { through, .. } => through,
            _ => continue,
        };
        if !table_exists_here(pool, table).await {
            let indexes = mig.forward.iter().filter_map(|op| match op {
                Operation::Schema(c @ SC::CreateIndex { table: t, .. }) if t == table => {
                    Some(c.clone())
                }
                _ => None,
            });
            groups.push(std::iter::once(change.clone()).chain(indexes).collect());
            continue;
        }
        let (SC::CreateTable(_), Some(snap)) = (change, mig.snapshot.table(table)) else {
            continue;
        };
        let live = super::ensure::live_columns(pool, table).await?;
        groups.extend(
            snap.fields
                .iter()
                .filter(|f| !live.contains(&f.column))
                .map(|f| {
                    vec![SC::AddColumn {
                        table: table.clone(),
                        column: f.column.clone(),
                    }]
                }),
        );
        // A create that already exists is skipped, so every index is offered (#2016).
        groups.extend(
            mig.snapshot
                .indexes
                .iter()
                .filter(|i| &i.table == table)
                .map(|i| vec![super::diff::create_index(i)]),
        );
        // Dropping a column loses data, so a leftover one is reported, not dropped.
        for column in live.iter().filter(|c| snap.field(c).is_none()) {
            tracing::warn!(
                target: "rustango::migrate",
                "`{table}.{column}` is not in the framework schema; drop it by hand \
                 if it is NOT NULL with no default, or inserts fail"
            );
        }
    }
    let failed = super::ensure::converge_groups(pool, &mig.snapshot, &groups).await?;
    warn_on_index_clash(pool, mig).await?;
    Ok(failed)
}

/// An index create that hit an existing name was skipped; warn when that
/// live index is on another table or other columns (#2016).
async fn warn_on_index_clash(pool: &crate::sql::Pool, mig: &Migration) -> Result<(), MigrateError> {
    let schema = super::ensure::creation_schema(pool)
        .await?
        .unwrap_or_default();
    for idx in &mig.snapshot.indexes {
        let live = super::inspectdb::index_columns(pool, &schema, &idx.table, &idx.name).await?;
        let Some((table, _)) = live.first() else {
            continue;
        };
        let columns: Vec<&str> = live.iter().map(|(_, c)| c.as_str()).collect();
        if table != &idx.table || columns != idx.columns {
            tracing::warn!(
                target: "rustango::migrate",
                "index `{}` is on `{table}` ({}), not `{}` ({}); drop or rename it by hand",
                idx.name,
                columns.join(", "),
                idx.table,
                idx.columns.join(", "),
            );
        }
    }
    Ok(())
}

/// How many of `tables` already exist in `pool`. Probes with
/// `SELECT 1 FROM <t> LIMIT 1` so each name resolves through exactly the
/// same search_path / current-database rules the migration's own
/// `CREATE TABLE` would use (unqualified — correct for schema-mode
/// tenants and single-DB alike).
async fn count_existing_tables(pool: &crate::sql::Pool, tables: &[String]) -> usize {
    let mut n = 0;
    for t in tables {
        if table_exists_here(pool, t).await {
            n += 1;
        }
    }
    n
}

/// Does `table` exist **in the schema this connection writes to**?
///
/// Deliberately *not* `SELECT 1 FROM <table>`: an unqualified name resolves
/// through the search path, so in schema-mode multi-tenancy (`search_path =
/// <tenant>, public`) a probe would happily find a same-named table in
/// `public` and conclude the tenant already had one. Reconciliation would then
/// skip creating it and the tenant would silently come up without its tables.
///
/// Each backend is asked about its *current* namespace only:
/// * Postgres — `information_schema.tables` filtered to `current_schema()`
/// * MySQL — filtered to `DATABASE()` (a schema is a database here)
/// * SQLite — `sqlite_master`, which is inherently per-connection
pub(crate) async fn table_exists_here(pool: &crate::sql::Pool, table: &str) -> bool {
    try_table_exists_here(pool, table).await.unwrap_or(false)
}

/// [`table_exists_here`], with a failed probe as an error rather than "missing".
pub(crate) async fn try_table_exists_here(
    pool: &crate::sql::Pool,
    table: &str,
) -> Result<bool, sqlx::Error> {
    let n = match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            // Indexed catalog lookup: sign-in runs it per request (#2366).
            // Checks `current_schema()` only, never the rest of `search_path`.
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = current_schema() AND c.relname = $1 \
             AND c.relkind IN ('r', 'p', 'v', 'f')",
            )
            .bind(table)
            .fetch_one(pg)
            .await?
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = ?",
            )
            .bind(table)
            .fetch_one(my)
            .await?
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_one(sq)
            .await?
        }
    };
    Ok(n > 0)
}

/// Record `mig` as applied **without running its `forward` ops**, and
/// tombstone every migration it `replaces`.
///
/// The squash row is inserted *before* the replaced rows are deleted, so a
/// crash midway leaves the ledger over-full (squash + some predecessors)
/// rather than empty — the next run then sees a full/partial state it can
/// report, instead of silently re-running the squash against live tables.
async fn fake_apply_pool(
    pool: &crate::sql::Pool,
    mig: &Migration,
    ledger: &str,
) -> Result<(), MigrateError> {
    // Render both statements before the first `.await` so the
    // `&dyn Dialect` borrow never crosses a suspend point (keeps this
    // future `Send` — see `count_existing_tables`).
    let (insert, delete) = {
        let dialect = pool.dialect();
        let placeholder = dialect.placeholder(1);
        let table = dialect.quote_ident(ledger);
        let name_col = dialect.quote_ident("name");
        let conflict_tail = dialect.insert_on_conflict_skip(&[&name_col]);
        (
            format!("INSERT INTO {table} ({name_col}) VALUES ({placeholder}) {conflict_tail}"),
            format!("DELETE FROM {table} WHERE {name_col} = {placeholder}"),
        )
    };
    crate::sql::raw_execute_pool(
        pool,
        &insert,
        vec![crate::core::SqlValue::String(mig.name.clone())],
    )
    .await
    .map_err(|e| {
        MigrateError::Validation(format!(
            "fake-apply: ledger insert for `{}` failed: {e}",
            mig.name
        ))
    })?;

    if mig.replaces.is_empty() {
        return Ok(());
    }
    for replaced in &mig.replaces {
        crate::sql::raw_execute_pool(
            pool,
            &delete,
            vec![crate::core::SqlValue::String(replaced.clone())],
        )
        .await
        .map_err(|e| {
            MigrateError::Validation(format!(
                "fake-apply: tombstoning `{replaced}` (replaced by `{}`) failed: {e}",
                mig.name
            ))
        })?;
    }
    Ok(())
}

/// Hold the migrate session-scoped advisory lock while `body` runs,
/// then release. Bi-dialect counterpart of [`with_migrate_lock`] —
/// dispatches the lock acquire/release SQL through the pool's dialect.
///
/// Backend-specific bind shapes:
/// - **Postgres** — `pg_advisory_lock($1)` takes an `i64`; we bind
///   [`MIGRATE_LOCK_KEY`] (the same key the legacy PgPool runner
///   uses, so the two paths coordinate).
/// - **MySQL** — `GET_LOCK` names are server-wide, so the dialect
///   appends a hash of `DATABASE()` to the bound `rustango_migrate_`
///   prefix: one lock per database, like PG advisory locks.
///
/// The lock is acquired on a checked-out connection and held until
/// `body` returns; release happens on the same connection so MySQL's
/// connection-scoped `GET_LOCK` semantics work correctly.
///
/// `ledger` is created under the lock before `body` runs (#1844).
async fn with_migrate_lock_pool<F, R>(
    pool: &crate::sql::Pool,
    ledger: &str,
    body: F,
) -> Result<R, MigrateError>
where
    F: std::future::Future<Output = Result<R, MigrateError>>,
{
    hold_migrate_lock(pool, async {
        create_ledger_locked(pool, ledger).await?;
        body.await
    })
    .await
}

/// Proof the caller holds the migrate lock; only [`with_migrate_lock_held`]
/// mints one, so the `*_locked` runners can't run unlocked.
#[derive(Clone, Copy)]
pub(crate) struct LockHeld(());

/// Run `body` under one migrate lock. Locks don't nest across pooled
/// connections, so `body` must call the `LockHeld` runners, not re-lock.
pub(crate) async fn with_migrate_lock_held<F, Fut, R>(
    pool: &crate::sql::Pool,
    body: F,
) -> Result<R, MigrateError>
where
    F: FnOnce(LockHeld) -> Fut,
    Fut: std::future::Future<Output = Result<R, MigrateError>>,
{
    hold_migrate_lock(pool, body(LockHeld(()))).await
}

tokio::task_local! {
    /// Set while this task holds the migrate lock.
    static LOCK_HELD: ();
    /// How long migrates in this task wait for the lock; unset waits forever.
    static LOCK_TIMEOUT: std::time::Duration;
}

/// Run `fut` with migrates that give up waiting for another run's lock after
/// `timeout`, with [`MigrateError::LockTimeout`]. SQLite takes no lock, so it never times out.
pub async fn with_lock_timeout<F: std::future::Future>(
    timeout: std::time::Duration,
    fut: F,
) -> F::Output {
    LOCK_TIMEOUT.scope(timeout, fut).await
}

/// MySQL `GET_LOCK`: 1 taken, 0 held elsewhere, NULL a server-side error.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
fn mysql_lock_taken(got: Option<i64>) -> Result<bool, sqlx::Error> {
    got.map(|n| n == 1).ok_or_else(|| {
        sqlx::Error::Protocol("GET_LOCK returned NULL: the server could not take the lock".into())
    })
}

/// A second lock from the task holding it would wait on itself forever.
fn refuse_nested_lock() -> Result<(), MigrateError> {
    if LOCK_HELD.try_with(|()| ()).is_ok() {
        return Err(MigrateError::Validation(
            "migrate called while this task holds the migrate lock; \
             run it after the outer migrate returns"
                .into(),
        ));
    }
    Ok(())
}

async fn hold_migrate_lock<F, R>(pool: &crate::sql::Pool, body: F) -> Result<R, MigrateError>
where
    F: std::future::Future<Output = Result<R, MigrateError>>,
{
    refuse_nested_lock()?;
    let body = LOCK_HELD.scope((), body);
    let dialect = pool.dialect();
    let (Some(acquire), Some(release)) = (
        dialect.acquire_session_lock_sql(),
        dialect.release_session_lock_sql(),
    ) else {
        // SQLite: one writer, so the database file lock serializes runs.
        return body.await;
    };
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut held = LockSession::wait(pg, |conn| {
                let sql = acquire.clone();
                Box::pin(async move {
                    sqlx::query_scalar::<_, bool>(&sql)
                        .bind(MIGRATE_LOCK_KEY)
                        .fetch_one(&mut **conn)
                        .await
                })
            })
            .await?;
            let result = body.await;
            // Best-effort: PG releases on session close, and the session closes.
            let _ = sqlx::query(&release)
                .bind(MIGRATE_LOCK_KEY)
                .execute(&mut **held.conn())
                .await;
            result
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            const NAME: &str = "rustango_migrate_";
            let mut held = LockSession::wait(my, |conn| {
                let sql = acquire.clone();
                Box::pin(async move {
                    let got: Option<i64> = sqlx::query_scalar(&sql)
                        .bind(NAME)
                        .fetch_one(&mut **conn)
                        .await?;
                    mysql_lock_taken(got)
                })
            })
            .await?;
            let result = body.await;
            let _ = sqlx::query(&release)
                .bind(NAME)
                .execute(&mut **held.conn())
                .await;
            result
        }
        #[allow(unreachable_patterns)]
        _ => {
            let _ = (acquire, release);
            body.await
        }
    }
}

/// A lock-session connection: pooled again only after a clean failed try,
/// closed otherwise, so a cancelled run never pools a session holding the lock.
#[cfg(any(feature = "postgres", feature = "mysql"))]
struct LockSession<DB: sqlx::Database>(Option<sqlx::pool::PoolConnection<DB>>);

#[cfg(any(feature = "postgres", feature = "mysql"))]
type TryLock<'c> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, sqlx::Error>> + Send + 'c>>;

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl<DB: sqlx::Database> LockSession<DB> {
    /// Poll `try_lock` until it takes the lock. A waiting run holds no pool
    /// connection between tries, so the holder's body can borrow one (#2027).
    async fn wait<F>(pool: &sqlx::Pool<DB>, try_lock: F) -> Result<Self, MigrateError>
    where
        F: for<'c> Fn(&'c mut sqlx::pool::PoolConnection<DB>) -> TryLock<'c>,
    {
        use std::hash::{BuildHasher as _, Hasher as _};
        use std::time::{Duration, Instant};
        let limit = LOCK_TIMEOUT.try_with(|d| *d).ok();
        let start = Instant::now();
        let mut pause = Duration::from_millis(10);
        let mut logged = false;
        loop {
            let mut session = Self(Some(pool.acquire().await?));
            if try_lock(session.conn()).await? {
                return Ok(session);
            }
            drop(session.0.take());
            if !std::mem::replace(&mut logged, true) {
                tracing::info!(target: "rustango::migrate", "waiting for another run's migrate lock");
            }
            // Jitter, so waiters started together don't retry in step.
            let rand = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish();
            let sleep = pause / 2 + pause.mul_f64((rand % 1000) as f64 / 2000.0);
            if let Some(limit) = limit.filter(|l| start.elapsed() + sleep > *l) {
                return Err(MigrateError::LockTimeout(limit));
            }
            tokio::time::sleep(sleep).await;
            pause = (pause * 2).min(Duration::from_millis(500));
        }
    }

    fn conn(&mut self) -> &mut sqlx::pool::PoolConnection<DB> {
        self.0.as_mut().expect("lock session connection")
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl<DB: sqlx::Database> Drop for LockSession<DB> {
    fn drop(&mut self) {
        if let Some(conn) = self.0.as_mut() {
            conn.close_on_drop();
        }
    }
}

/// One schema op rendered for the runner.
struct Step {
    batch: super::diff::RenderedBatch,
    /// SQLite's retry for an AddColumn on a table with rows (#2017).
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    retry: Option<super::ensure::FilledTableRetry>,
    /// Live FKs on a column to drop by catalog name before the step.
    #[cfg_attr(not(any(feature = "mysql", feature = "postgres")), allow(dead_code))]
    drop_fks: Option<ColumnRef>,
    /// A dropped single-column UNIQUE, found by name in the catalog (#1676).
    #[cfg_attr(not(feature = "mysql"), allow(dead_code))]
    drop_unique: Option<ColumnRef>,
    /// A MySQL index drop, which the FKs it serves refuse (1553; #2244, #2326). Boxed: `Step`
    /// sits in every migrate future, near a debug test thread's 2 MiB stack.
    index_fks: Option<Box<IndexFks>>,
    /// The column whose FK the step adds back before the migration ends.
    readded_now: Option<(String, String)>,
}

/// The FKs a MySQL index drop must take off first, found in the catalog:
/// the one on the index's first column, and the composite FKs no other index
/// serves. They come back right after the drop, under the names at the op,
/// so a later alter or rename of the column finds them.
struct IndexFks {
    table: String,
    index: String,
    /// The table's columns at the op, with their FK to re-add. A column
    /// missing here goes away later, or an earlier op already defers its
    /// FK's re-add, so its FK just drops; `None` if the whole table goes.
    columns: Option<Vec<(String, Option<String>)>>,
    /// The table's composite FKs at the op by name, with their re-add; one
    /// missing here just drops; `None` if the whole table goes (#2326).
    composites: Option<Vec<(String, String)>>,
}

/// The indexes on `table` that start with `column` once `earlier` ran on
/// `before`: its own, less those dropped, plus those created.
fn leading_indexes_at<'a>(
    before: &'a SchemaSnapshot,
    earlier: &'a [Operation],
    table: &str,
    column: &str,
) -> Vec<&'a str> {
    indexes_at(before, earlier, table)
        .into_iter()
        .filter(|(_, cs)| cs.first().is_some_and(|c| c == column))
        .map(|(name, _)| name)
        .collect()
}

/// The declared indexes on `table` with their columns once `earlier` ran on `before`.
fn indexes_at<'a>(
    before: &'a SchemaSnapshot,
    earlier: &'a [Operation],
    table: &str,
) -> Vec<(&'a str, &'a [String])> {
    use super::SchemaChange as SC;
    let mut out: Vec<(&str, &[String])> = before
        .indexes
        .iter()
        .filter(|i| i.table == table)
        .map(|i| (i.name.as_str(), i.columns.as_slice()))
        .collect();
    for op in earlier {
        match op {
            Operation::Schema(SC::DropIndex { name, .. }) => out.retain(|(n, _)| n != name),
            Operation::Schema(SC::CreateIndex {
                name,
                table: t,
                columns,
                ..
            }) if t == table => out.push((name, columns)),
            _ => {}
        }
    }
    out
}

/// Whether an earlier op of the migration defers `table.column`'s FK
/// (re-)add, which a re-add at the DropIndex would then duplicate (1826).
fn fk_deferred_earlier(table: &str, column: &str, earlier: &[Operation]) -> bool {
    use super::SchemaChange as SC;
    earlier.iter().any(|op| match op {
        Operation::Schema(SC::CreateTable(t)) => t == table,
        Operation::Schema(
            SC::AddColumn {
                table: t,
                column: c,
            }
            | SC::AlterColumnType {
                table: t,
                column: c,
                ..
            }
            | SC::AlterColumnMaxLength {
                table: t,
                column: c,
                ..
            }
            | SC::AlterFkOnDelete {
                table: t,
                column: c,
                ..
            }
            | SC::RenameColumn {
                table: t,
                new_column: c,
                ..
            }
            | SC::AlterColumnUnique {
                table: t,
                column: c,
                unique: false,
            },
        ) => t == table && c == column,
        _ => false,
    })
}

/// What happens to the FK on an index's first column.
enum FkFate<'a> {
    /// No FK in the schema: leave a live one alone.
    Keep,
    /// Its column or table goes away later.
    Drop,
    Readd(&'a str),
}

impl IndexFks {
    /// For dropping `index` on `table`, `ops[at_op]`.
    fn at(
        table: &str,
        index: &str,
        ops: &[Operation],
        at_op: Option<usize>,
        after: &SchemaSnapshot,
        dialect: &dyn crate::sql::Dialect,
        schema: Option<&str>,
    ) -> Result<Self, MigrateError> {
        let (earlier, later) = at_op.map_or((&[][..], &[][..]), |i| (&ops[..i], &ops[i + 1..]));
        // Only a later DropTable means the table goes; any other failure to
        // build its shape at the op is an error, not a reason to drop FKs.
        let (columns, composites) = if dropped_later(table, later) {
            (None, None)
        } else {
            let (at, _) = super::rebuild::snapshot_at(table, later, after)
                .map_err(MigrateError::Validation)?;
            let at = at.table(table);
            let mut columns = at
                .map(|t| super::diff::column_fks(t, dialect, schema))
                .transpose()
                .map_err(MigrateError::Validation)?;
            if let Some(cs) = &mut columns {
                cs.retain(|(c, fk)| fk.is_none() || !fk_deferred_earlier(table, c, earlier));
            }
            let composites = at.map(|t| super::diff::composite_fks(t, dialect, schema));
            (columns, composites)
        };
        Ok(Self {
            table: table.to_owned(),
            index: index.to_owned(),
            columns,
            composites,
        })
    }

    /// The drops of the live composite FKs `names` only the index serves,
    /// then the re-adds of those still declared; an undeclared one stays.
    fn plan_composites(
        &self,
        names: &[String],
        dialect: &dyn crate::sql::Dialect,
    ) -> (Vec<String>, Vec<String>) {
        let (mut drops, mut readds) = (Vec::new(), Vec::new());
        for name in names {
            let readd = match &self.composites {
                None => None,
                Some(cs) => match cs.iter().find(|(n, _)| n == name) {
                    Some((_, sql)) => Some(sql.clone()),
                    None => continue,
                },
            };
            drops.extend(dialect.drop_foreign_key_sql(&self.table, name));
            readds.extend(readd);
        }
        (drops, readds)
    }

    fn fate(&self, column: &str) -> FkFate<'_> {
        let found = self
            .columns
            .as_ref()
            .and_then(|cs| cs.iter().find(|(c, _)| c == column));
        match found {
            None => FkFate::Drop,
            Some((_, None)) => FkFate::Keep,
            Some((_, Some(sql))) => FkFate::Readd(sql),
        }
    }

    /// The drops of the live FKs `names` on `column`, then the re-add of the
    /// declared one, which also comes back if a failed run lost it.
    fn plan(
        &self,
        column: &str,
        names: &[String],
        dialect: &dyn crate::sql::Dialect,
    ) -> (Vec<String>, Option<String>) {
        let fate = self.fate(column);
        if matches!(fate, FkFate::Keep) {
            return (Vec::new(), None);
        }
        let drops = names
            .iter()
            .filter_map(|n| dialect.drop_foreign_key_sql(&self.table, n))
            .collect();
        match fate {
            FkFate::Readd(sql) => (drops, Some(sql.to_owned())),
            _ => (drops, None),
        }
    }
}

/// Whether a later op drops `table`, under whatever name it has by then.
fn dropped_later(table: &str, later: &[Operation]) -> bool {
    use super::SchemaChange as SC;
    let mut name = table.to_owned();
    for op in later {
        match op {
            Operation::Schema(SC::RenameTable { old_name, new_name }) if *old_name == name => {
                name.clone_from(new_name);
            }
            Operation::Schema(SC::DropTable(t)) if *t == name => return true,
            _ => {}
        }
    }
    false
}

/// A column whose FKs or UNIQUE the runner finds by name in the catalog:
/// a dropped column's FKs, which MySQL keeps (1828) (#1981), one being
/// replaced (#1557), or an index under an altered column (#1676).
#[cfg_attr(not(any(feature = "mysql", feature = "postgres")), allow(dead_code))]
struct ColumnRef {
    table: String,
    column: String,
    /// Declared indexes that stay.
    keep: Vec<String>,
}

/// Render `change`, one of `ops`, which together move the schema to `after`.
fn render_step(
    change: &super::SchemaChange,
    ops: &[Operation],
    after: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> Result<Step, MigrateError> {
    use super::SchemaChange as SC;
    // The ops after `change`, which is borrowed from `ops`.
    let at_op = ops
        .iter()
        .position(|op| matches!(op, Operation::Schema(c) if std::ptr::eq(c, change)));
    let later = at_op.map_or(&[][..], |i| &ops[i + 1..]);
    // Its FK is deferred past the later renames, so it takes their names (#2190).
    let ended = match change {
        SC::AddCompositeFk {
            table,
            name,
            to,
            from,
            on,
        } if !dialect.alters_by_rebuild() => Some(SC::AddCompositeFk {
            table: super::rebuild::name_at_end(table, later),
            name: name.clone(),
            to: super::rebuild::name_at_end(to, later),
            from: super::rebuild::columns_at_end(table, from, later),
            on: super::rebuild::columns_at_end(to, on, later),
        }),
        _ => None,
    };
    let changes = std::slice::from_ref(ended.as_ref().unwrap_or(change));
    let render = |snap: &SchemaSnapshot| {
        super::diff::render_changes_split_in_schema(changes, snap, dialect, schema)
    };
    let retry = super::ensure::filled_table_retry(dialect, after, changes, render)
        .transpose()
        .map_err(MigrateError::Validation)?;
    // The changes SQLite makes by rebuilding the table.
    let rebuilt = match change {
        SC::DropColumn { table, .. }
        | SC::AlterFkOnDelete { table, .. }
        | SC::AlterColumnType { table, .. }
        | SC::AlterColumnNullable { table, .. }
        | SC::AlterColumnDefault { table, .. }
        | SC::AlterColumnMaxLength { table, .. }
        | SC::AlterColumnUnique { table, .. }
        | SC::AddCheckConstraint { table, .. }
        | SC::DropCheckConstraint { table, .. }
        | SC::AddCompositeFk { table, .. }
        | SC::DropCompositeFk { table, .. } => Some(table.as_str()),
        _ => None,
    };
    // The changes MySQL makes by restating the whole column.
    let modified = match change {
        SC::AlterColumnType { table, .. }
        | SC::AlterColumnNullable { table, .. }
        | SC::AlterColumnDefault { table, .. }
        | SC::AlterColumnMaxLength { table, .. }
        | SC::AlterColumnUnique { table, .. } => Some(table.as_str()),
        _ => None,
    };
    let reshaped = match change {
        // Every backend looks up their FK or declared indexes by table name.
        SC::AlterFkOnDelete { table, .. }
        | SC::AlterColumnUnique {
            table,
            unique: false,
            ..
        } => Some(table.as_str()),
        _ if dialect.alters_by_rebuild() => rebuilt,
        _ => modified.filter(|_| dialect.modifies_whole_column()),
    };
    // Both take the table's shape at this op, not at the end (#2121, #2149).
    let (at, renamed) = match reshaped {
        Some(table) if later.iter().any(|op| touches_table(op, table)) => {
            let (at, renamed) = super::rebuild::snapshot_at(table, later, after)
                .map_err(MigrateError::Validation)?;
            (Some(at), renamed)
        }
        _ => (None, false),
    };
    let snap = at.as_ref().unwrap_or(after);
    let at_column = |table: &str, column: &str| ColumnRef {
        table: table.to_owned(),
        column: column.to_owned(),
        keep: super::diff::declared_indexes(snap, table),
    };
    let has_fk = |table: &str, column: &str| {
        snap.table(table)
            .and_then(|t| t.field(column))
            .is_some_and(|f| f.fk.is_some())
    };
    // A rename keeps the FK's old name, so it comes back under the new one,
    // with the names at the end, once its last rename ran (#2307).
    let renamed_fk = match change {
        SC::RenameColumn {
            table, new_column, ..
        } if !dialect.inline_fks_in_create_table()
            && !fk_readded_later(table, new_column, later, dialect) =>
        {
            let end = super::rebuild::name_at_end(table, later);
            let at_end =
                super::rebuild::columns_at_end(table, std::slice::from_ref(new_column), later);
            super::diff::column_fk_sql(after, &end, &at_end[0], dialect, schema)
                .map_err(MigrateError::Validation)?
                .map(|sql| (at_end[0] == *new_column, sql))
        }
        _ => None,
    };
    let drop_fks = match change {
        SC::DropColumn { table, column } | SC::AlterFkOnDelete { table, column, .. } => {
            Some(at_column(table, column))
        }
        SC::RenameColumn {
            table, old_column, ..
        } if renamed_fk.is_some() => Some(at_column(table, old_column)),
        _ => readds_fk(change, dialect)
            .filter(|(table, column)| has_fk(table, column))
            .map(|(table, column)| at_column(table, column)),
    };
    let drop_unique = match change {
        SC::AlterColumnUnique {
            table,
            column,
            unique: false,
        } => Some(at_column(table, column)),
        _ => None,
    };
    let index_fks = match change {
        SC::DropIndex { name, table } if dialect.sole_leading_column_sql().is_some() => {
            Some(Box::new(IndexFks::at(
                table, name, ops, at_op, after, dialect, schema,
            )?))
        }
        _ => None,
    };
    let mut batch = render(snap).map_err(MigrateError::Validation)?;
    if let Some((true, sql)) = renamed_fk {
        batch.deferred_fks.push(sql);
    }
    // The FK it re-adds, whose column a later rename takes off and re-adds.
    let column_renamed = !dialect.inline_fks_in_create_table()
        && readds_fk(change, dialect).is_some_and(|(table, column)| {
            super::rebuild::columns_at_end(table, &[column.to_owned()], later)[0] != column
        });
    let readded_now = readds_fk(change, dialect)
        .filter(|_| (renamed || column_renamed) && drop_fks.is_some())
        .map(|(t, c)| (t.to_owned(), c.to_owned()));
    // Its FKs carry the names at this op, which a later rename changes.
    if renamed || column_renamed {
        let fks = std::mem::take(&mut batch.deferred_fks);
        batch.immediate.extend(fks);
    }
    // A rebuild already leaves every column the later ops drop.
    batch.rebuild = batch.rebuild.map(|r| {
        let dropped: Vec<&str> = later
            .iter()
            .filter_map(|op| match op {
                Operation::Schema(super::SchemaChange::DropColumn { table, column })
                    if table == r.table() =>
                {
                    Some(column.as_str())
                }
                _ => None,
            })
            .collect();
        dropped.into_iter().fold(r, |r, c| r.dropping(c))
    });
    Ok(Step {
        batch,
        retry,
        drop_fks,
        drop_unique,
        index_fks,
        readded_now,
    })
}

/// The column whose live FK `change` drops and adds back: ON DELETE
/// everywhere, and MySQL's MODIFY, which refuses it on (3780, 1553).
fn readds_fk<'a>(
    change: &'a super::SchemaChange,
    dialect: &dyn crate::sql::Dialect,
) -> Option<(&'a str, &'a str)> {
    use super::SchemaChange as SC;
    match change {
        SC::AlterFkOnDelete { table, column, .. } => Some((table, column)),
        SC::AlterColumnType { table, column, .. }
        | SC::AlterColumnMaxLength { table, column, .. }
        | SC::AlterColumnUnique {
            table,
            column,
            unique: false,
        } if dialect.modifies_whole_column() => Some((table, column)),
        _ => None,
    }
}

/// Whether one of `later` re-adds the FK of `table.column`, so a rename
/// leaves it to that op rather than adding it twice.
fn fk_readded_later(
    table: &str,
    column: &str,
    later: &[Operation],
    dialect: &dyn crate::sql::Dialect,
) -> bool {
    later.iter().enumerate().any(|(i, op)| {
        let Operation::Schema(change) = op else {
            return false;
        };
        readds_fk(change, dialect).is_some_and(|(t, c)| {
            let before = &later[..i];
            t == super::rebuild::name_at_end(table, before)
                && super::rebuild::columns_at_end(table, &[column.to_owned()], before)[0] == c
        })
    })
}

/// Whether `op` changes `table` or renames it.
fn touches_table(op: &Operation, table: &str) -> bool {
    matches!(op, Operation::Schema(c) if c.table() == table)
}

/// Whether any of `ops` rebuilds a table, so FK enforcement must go off.
#[cfg(feature = "sqlite")]
fn rebuilds(ops: &[Operation], after: &SchemaSnapshot) -> Result<bool, MigrateError> {
    for op in ops {
        if let Operation::Schema(change) = op {
            if render_step(change, ops, after, &crate::sql::Sqlite, None)?
                .batch
                .rebuild
                .is_some()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Run `step` on SQLite, retrying an ADD COLUMN refused for a non-constant
/// DEFAULT, as `ensure` does. Returns the deferred FK statements.
#[cfg(feature = "sqlite")]
async fn run_step_sqlite(
    tx: &mut super::rebuild::RebuildTx<'_>,
    step: Step,
) -> Result<Vec<String>, MigrateError> {
    let Step { batch, retry, .. } = step;
    for (i, stmt) in batch.immediate.iter().enumerate() {
        let Err(e) = sqlx::query(stmt).execute(&mut **tx).await else {
            continue;
        };
        // The ADD COLUMN is the first statement; nothing ran before it.
        let Some(retry) = retry.filter(|_| i == 0 && super::ensure::is_non_constant_default(&e))
        else {
            return Err(e.into());
        };
        tracing::warn!(target: "rustango::migrate", "column {}", retry.warning);
        for stmt in &retry.batch.immediate {
            sqlx::query(stmt).execute(&mut **tx).await?;
        }
        return Ok(retry.batch.deferred_fks);
    }
    if let Some(rebuild) = &batch.rebuild {
        rebuild.run(tx).await?;
    }
    Ok(batch.deferred_fks)
}

/// `step`'s statements on `$conn`, after dropping its live FKs by their
/// catalog names, which renames and old 64-byte names don't match.
#[cfg(any(feature = "mysql", feature = "postgres"))]
macro_rules! step_statements {
    ($conn:expr, $step:expr, $dialect:expr) => {{
        use crate::sql::Dialect as _;
        let (step, dialect): (&Step, _) = ($step, $dialect);
        let mut out = Vec::new();
        if let (Some(fk), Some(sql)) = (&step.drop_fks, dialect.foreign_key_names_sql()) {
            let names: Vec<String> = sqlx::query_scalar(sql)
                .bind(&fk.table)
                .bind(&fk.column)
                .fetch_all(&mut *$conn)
                .await?;
            out.extend(
                names
                    .iter()
                    .filter_map(|n| dialect.drop_foreign_key_sql(&fk.table, n)),
            );
        }
        if let (Some(u), Some(sql)) = (&step.drop_unique, dialect.unique_index_names_sql()) {
            let names: Vec<String> = sqlx::query_scalar(sql)
                .bind(&u.table)
                .bind(&u.column)
                .fetch_all(&mut *$conn)
                .await?;
            out.extend(
                names
                    .iter()
                    .filter(|n| !u.keep.contains(n))
                    .filter_map(|n| dialect.drop_unique_index_sql(&u.table, n)),
            );
        }
        out.extend(step.batch.immediate.iter().cloned());
        Ok(out)
    }};
}

#[cfg(feature = "mysql")]
async fn mysql_statements(
    conn: &mut sqlx::MySqlConnection,
    step: &Step,
) -> Result<Vec<String>, sqlx::Error> {
    let (mut out, readd) = match &step.index_fks {
        Some(ix) => Box::pin(mysql_index_fks(conn, ix)).await?,
        None => (Vec::new(), Vec::new()),
    };
    let rest: Result<Vec<String>, sqlx::Error> = step_statements!(conn, step, crate::sql::MySql);
    out.extend(rest?);
    out.extend(readd);
    Ok(out)
}

/// [`IndexFks::plan`] for the index's first column, when no other index
/// starts with it: then MySQL needs it for the FK (1553), and otherwise a
/// plain drop works without the FK's table-copy re-add. Then the same for
/// the composite FKs only it serves. Boxed by the caller.
#[cfg(feature = "mysql")]
async fn mysql_index_fks(
    conn: &mut sqlx::MySqlConnection,
    ix: &IndexFks,
) -> Result<(Vec<String>, Vec<String>), sqlx::Error> {
    use crate::sql::Dialect as _;
    let dialect = crate::sql::MySql;
    let (Some(lead_sql), Some(names_sql), Some(composite_sql)) = (
        dialect.sole_leading_column_sql(),
        dialect.foreign_key_names_sql(),
        dialect.composite_fks_needing_index_sql(),
    ) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let lead: Option<String> = sqlx::query_scalar(lead_sql)
        .bind(&ix.table)
        .bind(&ix.index)
        .fetch_optional(&mut *conn)
        .await?;
    let (mut drops, mut readds) = (Vec::new(), Vec::new());
    if let Some(column) = lead {
        let names: Vec<String> = sqlx::query_scalar(names_sql)
            .bind(&ix.table)
            .bind(&column)
            .fetch_all(&mut *conn)
            .await?;
        let (d, r) = ix.plan(&column, &names, &dialect);
        drops.extend(d);
        readds.extend(r);
    }
    let names: Vec<String> = sqlx::query_scalar(composite_sql)
        .bind(&ix.table)
        .bind(&ix.index)
        .bind(&ix.index)
        .fetch_all(&mut *conn)
        .await?;
    let (d, r) = ix.plan_composites(&names, &dialect);
    drops.extend(d);
    readds.extend(r);
    Ok((drops, readds))
}

#[cfg(feature = "postgres")]
async fn pg_statements(
    conn: &mut sqlx::PgConnection,
    step: &Step,
) -> Result<Vec<String>, sqlx::Error> {
    step_statements!(conn, step, crate::sql::Postgres)
}

/// Where a migration's transaction records it in the ledger.
#[cfg(any(feature = "sqlite", feature = "mysql"))]
#[derive(Clone, Copy)]
enum LedgerWrite<'a> {
    Insert(&'a str),
    Delete(&'a str),
}

/// Run `ops` on MySQL in a transaction that only covers data ops before the
/// first DDL; a failure after it is `PartiallyApplied` (#1588, #2151).
#[cfg(feature = "mysql")]
async fn atomic_mysql(
    my: &sqlx::MySqlPool,
    name: &str,
    ops: &[Operation],
    after: &SchemaSnapshot,
    schema: Option<&str>,
    ledger: LedgerWrite<'_>,
) -> Result<(), MigrateError> {
    let mut tx = my.begin().await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    // What has already committed when something fails.
    //
    // Once a DDL statement was sent, every finished operation
    // is committed, so a failure then is reported with what it
    // left behind (#1588, #2151). Before that it rolls back.
    let total = ops.len();
    let mut applied = 0usize;
    let mut ddl_applied = 0usize;
    let mut ddl_sent = false;
    // Wrap a driver error with what survived it.
    macro_rules! stuck {
        ($e:expr) => {{
            let e: sqlx::Error = $e;
            if !ddl_sent || (applied == 0 && ddl_applied == 0) {
                MigrateError::Driver(e)
            } else {
                MigrateError::PartiallyApplied {
                    migration: name.to_owned(),
                    applied,
                    total,
                    ddl_applied,
                    source: Box::new(e),
                }
            }
        }};
    }
    for op in ops {
        match op {
            Operation::Schema(change) => {
                let step = render_step(change, ops, after, &crate::sql::MySql, schema)?;
                let stmts = mysql_statements(&mut tx, &step)
                    .await
                    .map_err(|e| stuck!(e))?;
                // Counted per *statement*, not per operation:
                // one operation can render several, and each
                // auto-commits on its own, so an operation
                // that fails halfway has still left the
                // earlier ones applied.
                for stmt in stmts {
                    // DDL that fails to parse does not commit;
                    // commit first so the count holds either way.
                    if !ddl_sent {
                        tx.commit().await?;
                        // Committed now: a failed `begin` must say so.
                        ddl_sent = true;
                        tx = my.begin().await.map_err(|e| stuck!(e))?;
                    }
                    sqlx::query(&stmt)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| stuck!(e))?;
                    ddl_applied += 1;
                }
                deferred_fks.extend(step.batch.deferred_fks);
            }
            Operation::Data(d) => {
                sqlx::query(&d.sql)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| stuck!(e))?;
            }
            Operation::Callback(c) => return Err(callback_in_atomic(name, c)),
        }
        applied += 1;
    }
    for stmt in deferred_fks {
        sqlx::query(&stmt)
            .execute(&mut *tx)
            .await
            .map_err(|e| stuck!(e))?;
    }
    match ledger {
        LedgerWrite::Insert(ledger) => sqlx::query(&ledger_insert_sql(&crate::sql::MySql, ledger))
            .bind(name)
            .bind(chrono::Utc::now())
            .execute(&mut *tx)
            .await
            .map_err(|e| stuck!(e))?,
        LedgerWrite::Delete(ledger) => sqlx::query(&format!("DELETE FROM {ledger} WHERE name = ?"))
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| stuck!(e))?,
    };
    tx.commit().await.map_err(|e| stuck!(e))?;
    Ok(())
}

/// Run `ops` on SQLite in one transaction, FK enforcement off around it
/// when one of them rebuilds a table.
#[cfg(feature = "sqlite")]
async fn atomic_sqlite(
    sq: &sqlx::SqlitePool,
    name: &str,
    ops: &[Operation],
    after: &SchemaSnapshot,
    ledger: LedgerWrite<'_>,
) -> Result<(), MigrateError> {
    let rebuilds = rebuilds(ops, after)?;
    // FK enforcement can only change outside a transaction, so it is off for
    // the whole migration; a RunSQL would lose its ON DELETE actions.
    if rebuilds && ops.iter().any(|op| matches!(op, Operation::Data(_))) {
        return Err(MigrateError::Validation(format!(
            "migration `{name}` rebuilds a SQLite table, which runs with FOREIGN KEY \
             enforcement off; move its RunSQL to a migration of its own or set `atomic: false`"
        )));
    }
    let mut conn = super::rebuild::RebuildConn::acquire(sq, rebuilds).await?;
    let result: Result<(), MigrateError> = async {
        let mut tx = conn.begin().await?;
        let mut deferred_fks: Vec<String> = Vec::new();
        for op in ops {
            match op {
                Operation::Schema(change) => {
                    let step = render_step(change, ops, after, &crate::sql::Sqlite, None)?;
                    deferred_fks.extend(run_step_sqlite(&mut tx, step).await?);
                }
                Operation::Data(d) => {
                    sqlx::query(&d.sql).execute(&mut *tx).await?;
                }
                Operation::Callback(c) => return Err(callback_in_atomic(name, c)),
            }
        }
        for stmt in deferred_fks {
            sqlx::query(&stmt).execute(&mut *tx).await?;
        }
        match ledger {
            // `encode_datetime`, not a bare `DateTime<Utc>`: sqlx-sqlite
            // has its own RFC3339 formatter with a variable-width
            // fraction, which is a second spelling of the same instant.
            LedgerWrite::Insert(ledger) => {
                sqlx::query(&ledger_insert_sql(&crate::sql::Sqlite, ledger))
                    .bind(name)
                    .bind(crate::sql::encode_datetime(chrono::Utc::now()))
                    .execute(&mut *tx)
                    .await?;
            }
            LedgerWrite::Delete(ledger) => {
                sqlx::query(&format!("DELETE FROM {ledger} WHERE name = ?"))
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await
    }
    .await;
    // The migration's own error wins over a failed restore.
    let finished = conn.finish().await;
    result?;
    Ok(finished?)
}

/// Run `step` for the non-atomic runners, on any backend.
async fn run_step_pool(pool: &crate::sql::Pool, step: Step) -> Result<Vec<String>, MigrateError> {
    match pool {
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            // A rebuild is several statements; it gets a transaction of its own.
            let mut conn =
                super::rebuild::RebuildConn::acquire(sq, step.batch.rebuild.is_some()).await?;
            let result: Result<Vec<String>, MigrateError> = async {
                let mut tx = conn.begin().await?;
                let deferred = run_step_sqlite(&mut tx, step).await?;
                tx.commit().await?;
                Ok(deferred)
            }
            .await;
            let finished = conn.finish().await;
            let deferred = result?;
            finished?;
            Ok(deferred)
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut conn = my.acquire().await?;
            for stmt in mysql_statements(&mut conn, &step).await? {
                sqlx::query(&stmt).execute(&mut *conn).await?;
            }
            Ok(step.batch.deferred_fks)
        }
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut conn = pg.acquire().await?;
            for stmt in pg_statements(&mut conn, &step).await? {
                sqlx::query(&stmt).execute(&mut *conn).await?;
            }
            Ok(step.batch.deferred_fks)
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
    #[cfg_attr(
        not(any(feature = "postgres", feature = "mysql")),
        allow(unused_variables)
    )]
    let schema = super::ensure::creation_schema(pool).await?;
    #[cfg_attr(not(feature = "postgres"), allow(unused_variables))]
    let dialect = pool.dialect();
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in &mig.forward {
                match op {
                    Operation::Schema(change) => {
                        let step = render_step(
                            change,
                            &mig.forward,
                            &mig.snapshot,
                            dialect,
                            schema.as_deref(),
                        )?;
                        for stmt in pg_statements(&mut tx, &step).await? {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(step.batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => return Err(callback_in_atomic(&mig.name, c)),
                }
            }
            for stmt in deferred_fks {
                sqlx::query(&stmt).execute(&mut *tx).await?;
            }
            sqlx::query(&ledger_insert_sql(&crate::sql::Postgres, ledger))
                .bind(&mig.name)
                .bind(chrono::Utc::now())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            // MySQL commits implicitly on every DDL statement and does not
            // roll DDL back (#559). `atomic_mysql`'s BEGIN covers only data ops
            // before the first DDL; every op after it runs in autocommit (#1660).
            tracing::warn!(
                migration = %mig.name,
                "MySQL commits on every DDL statement: the atomic wrapper only covers \
                 data operations before the first DDL. A failure after it leaves the \
                 migration partially applied and needs manual recovery (#559)."
            );
            atomic_mysql(
                my,
                &mig.name,
                &mig.forward,
                &mig.snapshot,
                schema.as_deref(),
                LedgerWrite::Insert(ledger),
            )
            .await?;
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            atomic_sqlite(
                sq,
                &mig.name,
                &mig.forward,
                &mig.snapshot,
                LedgerWrite::Insert(ledger),
            )
            .await?;
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
    let schema = super::ensure::creation_schema(pool).await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in &mig.forward {
        match op {
            Operation::Schema(change) => {
                let step = render_step(
                    change,
                    &mig.forward,
                    &mig.snapshot,
                    pool.dialect(),
                    schema.as_deref(),
                )?;
                deferred_fks.extend(run_step_pool(pool, step).await?);
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
    let insert_sql = ledger_insert_sql(pool.dialect(), ledger);
    crate::sql::raw_execute_pool(
        pool,
        &insert_sql,
        ::std::vec![
            crate::core::SqlValue::String(mig.name.clone()),
            crate::core::SqlValue::DateTime(chrono::Utc::now()),
        ],
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
    with_migrate_lock_pool(pool, ledger, async {
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
                for mig in forward_to(&all, &applied, None, target) {
                    reconcile_and_apply(pool, &mig, &applied, ledger, false).await?;
                    touched.push(mig);
                }
            }
            Some(h) => {
                use std::cmp::Ordering;
                match target.cmp(h.as_str()) {
                    Ordering::Equal => {}
                    Ordering::Greater => {
                        for mig in forward_to(&all, &applied, Some(&h), target) {
                            reconcile_and_apply(pool, &mig, &applied, ledger, false).await?;
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
    with_migrate_lock_pool(pool, ledger, async {
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
    with_migrate_lock_pool(pool, ledger, async {
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
    with_migrate_lock_pool(pool, ledger, unapply_locked_pool(pool, dir, name, ledger)).await
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
    let pending = all.iter().filter(|m| !applied.contains(&m.name));
    let mut out = Vec::new();
    for mig in pending {
        let before = prev_snapshot(&all, mig, dir)?;
        out.push(preview_migration(mig, &before, pool.dialect(), ledger)?);
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

    let prev_snapshot = prev_snapshot(&all, &target, dir)?;

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
    #[cfg_attr(
        not(any(feature = "postgres", feature = "mysql")),
        allow(unused_variables)
    )]
    let schema = super::ensure::creation_schema(pool).await?;
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let mut deferred_fks: Vec<String> = Vec::new();
            for op in inverted {
                match op {
                    Operation::Schema(change) => {
                        let step = render_step(
                            change,
                            inverted,
                            snapshot,
                            pool.dialect(),
                            schema.as_deref(),
                        )?;
                        for stmt in pg_statements(&mut tx, &step).await? {
                            sqlx::query(&stmt).execute(&mut *tx).await?;
                        }
                        deferred_fks.extend(step.batch.deferred_fks);
                    }
                    Operation::Data(d) => {
                        sqlx::query(&d.sql).execute(&mut *tx).await?;
                    }
                    Operation::Callback(c) => return Err(callback_in_atomic(&target.name, c)),
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
            // As in `apply_atomic_pool`: only ops before the first DDL are in the tx.
            tracing::warn!(
                migration = %target.name,
                "MySQL commits on every DDL statement: the atomic-unapply wrapper only \
                 covers data operations before the first DDL. A failure after it leaves \
                 the schema half-reverted and needs manual recovery (#559)."
            );
            atomic_mysql(
                my,
                &target.name,
                inverted,
                snapshot,
                schema.as_deref(),
                LedgerWrite::Delete(ledger),
            )
            .await?;
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            atomic_sqlite(
                sq,
                &target.name,
                inverted,
                snapshot,
                LedgerWrite::Delete(ledger),
            )
            .await?;
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
    with_migrate_lock_pool(pool, ledger, async {
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
            .iter()
            .filter(|m| !applied.contains(&m.name))
            .cloned()
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
    let schema = super::ensure::creation_schema(pool).await?;
    let mut deferred_fks: Vec<String> = Vec::new();
    for op in inverted {
        match op {
            Operation::Schema(change) => {
                let step = render_step(
                    change,
                    inverted,
                    snapshot,
                    pool.dialect(),
                    schema.as_deref(),
                )?;
                deferred_fks.extend(run_step_pool(pool, step).await?);
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

#[cfg(test)]
mod tests {
    /// GET_LOCK's NULL is a server error, not "held elsewhere": polling it would spin forever.
    #[test]
    fn mysql_get_lock_null_is_an_error() {
        assert!(super::mysql_lock_taken(Some(1)).unwrap());
        assert!(!super::mysql_lock_taken(Some(0)).unwrap());
        assert!(super::mysql_lock_taken(None).is_err());
    }

    /// #2026 — MySQL refuses to drop an FK column (1828); its FK goes first.
    #[cfg(feature = "mysql")]
    #[test]
    fn render_between_drops_the_fk_before_its_column() {
        use crate::migrate::{SchemaChange, SchemaSnapshot};
        let table = |with_fk: bool| {
            let mut fields = vec![
                serde_json::json!({ "name": "id", "column": "id", "ty": "i64",
                "nullable": false, "primary_key": true }),
            ];
            if with_fk {
                fields.push(
                    serde_json::json!({ "name": "p", "column": "p_id", "ty": "i64",
                    "nullable": true, "primary_key": false,
                    "fk": { "kind": "fk", "to": "parent", "on": "id" } }),
                );
            }
            serde_json::json!({ "name": "child", "model": "Child", "fields": fields })
        };
        let snap = |with_fk: bool| -> SchemaSnapshot {
            serde_json::from_value(serde_json::json!({ "tables": [table(with_fk)] })).unwrap()
        };
        let drop = [SchemaChange::DropColumn {
            table: "child".into(),
            column: "p_id".into(),
        }];
        let out =
            super::render_changes_between(&drop, &snap(true), &snap(false), &crate::sql::MySql)
                .unwrap();
        assert!(out[0].contains("DROP FOREIGN KEY"), "{out:?}");
        assert!(out[1].contains("DROP COLUMN `p_id`"), "{out:?}");
    }

    /// #2244 — the index an FK uses drops after the FK, which comes back.
    #[test]
    fn render_between_drops_the_fk_around_its_index() {
        use crate::migrate::{SchemaChange, SchemaSnapshot};
        let snap = |indexed: bool| -> SchemaSnapshot {
            let indexes = if indexed {
                serde_json::json!([{ "name": "child_p_idx", "table": "child",
                    "columns": ["p_id"], "unique": false }])
            } else {
                serde_json::json!([])
            };
            serde_json::from_value(serde_json::json!({ "tables": [{
                "name": "child", "model": "Child", "fields": [
                    { "name": "id", "column": "id", "ty": "i64",
                      "nullable": false, "primary_key": true },
                    { "name": "p", "column": "p_id", "ty": "i64",
                      "nullable": true, "primary_key": false,
                      "fk": { "kind": "fk", "to": "parent", "on": "id" } }] }],
                "indexes": indexes }))
            .unwrap()
        };
        let drop = [SchemaChange::DropIndex {
            name: "child_p_idx".into(),
            table: "child".into(),
        }];
        let out =
            super::render_changes_between(&drop, &snap(true), &snap(false), &crate::sql::MySql)
                .unwrap();
        assert!(out[0].contains("DROP FOREIGN KEY"), "{out:?}");
        assert!(out[1].starts_with("DROP INDEX"), "{out:?}");
        assert!(out[2].contains("ADD CONSTRAINT"), "{out:?}");
        let pg =
            super::render_changes_between(&drop, &snap(true), &snap(false), &crate::sql::Postgres)
                .unwrap();
        assert_eq!(pg.len(), 1, "PG needs no index under an FK: {pg:?}");
    }

    /// #2244 — the second of two index drops is the one the FK needs: the
    /// first no longer serves it once dropped.
    #[test]
    fn render_between_takes_the_fk_off_for_the_last_index() {
        use crate::migrate::{SchemaChange, SchemaSnapshot};
        let snap = |indexed: bool| -> SchemaSnapshot {
            let indexes = if indexed {
                serde_json::json!([
                    { "name": "idx_a", "table": "child", "columns": ["p_id", "x"], "unique": false },
                    { "name": "idx_b", "table": "child", "columns": ["p_id", "y"], "unique": false }])
            } else {
                serde_json::json!([])
            };
            serde_json::from_value(serde_json::json!({ "tables": [{
                "name": "child", "model": "Child", "fields": [
                    { "name": "id", "column": "id", "ty": "i64",
                      "nullable": false, "primary_key": true },
                    { "name": "p", "column": "p_id", "ty": "i64",
                      "nullable": true, "primary_key": false,
                      "fk": { "kind": "fk", "to": "parent", "on": "id" } }] }],
                "indexes": indexes }))
            .unwrap()
        };
        let drop = |name: &str| SchemaChange::DropIndex {
            name: name.into(),
            table: "child".into(),
        };
        let out = super::render_changes_between(
            &[drop("idx_a"), drop("idx_b")],
            &snap(true),
            &snap(false),
            &crate::sql::MySql,
        )
        .unwrap();
        assert!(out[0].starts_with("DROP INDEX `idx_a`"), "{out:?}");
        assert!(out[1].contains("DROP FOREIGN KEY"), "{out:?}");
        assert!(out[2].starts_with("DROP INDEX `idx_b`"), "{out:?}");
        assert!(out[3].contains("ADD CONSTRAINT"), "{out:?}");
        assert_eq!(out.len(), 4, "{out:?}");
    }

    /// #2326 — a composite FK only the dropped index serves comes off around it,
    /// though another index starts with its first column.
    #[test]
    fn render_between_drops_a_composite_fk_around_its_index() {
        use crate::migrate::{SchemaChange, SchemaSnapshot};
        // `other`: one more index on the table, which may serve the FK too.
        let snap = |indexed: bool, other: &[&str]| -> SchemaSnapshot {
            let mut indexes = vec![serde_json::json!({ "name": "child_p_idx",
                "table": "child", "columns": ["p_id"], "unique": false })];
            if indexed {
                indexes.push(
                    serde_json::json!({ "name": "child_pc_idx", "table": "child",
                    "columns": ["p_id", "code", "n"], "unique": false }),
                );
            }
            if !other.is_empty() {
                indexes.push(serde_json::json!({ "name": "child_other_idx",
                    "table": "child", "columns": other, "unique": false }));
            }
            let col = |c: &str| {
                serde_json::json!({ "name": c, "column": c, "ty": "i64",
                "nullable": true, "primary_key": false })
            };
            serde_json::from_value(serde_json::json!({ "tables": [{
                "name": "child", "model": "Child", "fields": [
                    { "name": "id", "column": "id", "ty": "i64",
                      "nullable": false, "primary_key": true },
                    col("p_id"), col("code"), col("n")],
                "composite_fks": [{ "name": "child_pc_fk", "to": "parent",
                    "from": ["p_id", "code"], "on": ["id", "code"] }] }],
                "indexes": indexes }))
            .unwrap()
        };
        let drop = [SchemaChange::DropIndex {
            name: "child_pc_idx".into(),
            table: "child".into(),
        }];
        let render = |other: &[&str]| {
            super::render_changes_between(
                &drop,
                &snap(true, other),
                &snap(false, other),
                &crate::sql::MySql,
            )
            .unwrap()
        };
        // `(code, p_id)` has the FK's columns in the wrong order.
        for other in [&[][..], &["code", "p_id"]] {
            let out = render(other);
            assert!(out[0].contains("DROP FOREIGN KEY `child_pc_fk`"), "{out:?}");
            assert!(out[1].starts_with("DROP INDEX"), "{out:?}");
            assert!(out[2].contains("ADD CONSTRAINT `child_pc_fk`"), "{out:?}");
            assert_eq!(out.len(), 3, "{out:?}");
        }
        let out = render(&["p_id", "code"]);
        assert_eq!(out.len(), 1, "another index serves it: {out:?}");
        assert!(out[0].starts_with("DROP INDEX"), "{out:?}");
    }

    /// #2307 — a renamed FK column's FK comes back under the new name, once
    /// even when a later op re-adds it.
    #[test]
    fn render_between_renames_an_fk_once() {
        use crate::migrate::{SchemaChange, SchemaSnapshot};
        let snap = |column: &str, on_delete: &str| -> SchemaSnapshot {
            serde_json::from_value(serde_json::json!({ "tables": [
                { "name": "author", "model": "Author", "fields": [
                    { "name": "id", "column": "id", "ty": "i64",
                      "nullable": false, "primary_key": true }] },
                { "name": "book", "model": "Book", "fields": [
                    { "name": "id", "column": "id", "ty": "i64",
                      "nullable": false, "primary_key": true },
                    { "name": column, "column": column, "ty": "i64", "nullable": true,
                      "primary_key": false, "fk": { "kind": "fk", "to": "author",
                      "on": "id", "on_delete": on_delete } }] }] }))
            .unwrap()
        };
        let rename = SchemaChange::RenameColumn {
            table: "book".into(),
            old_column: "author_id".into(),
            new_column: "writer_id".into(),
        };
        let on_delete = SchemaChange::AlterFkOnDelete {
            table: "book".into(),
            column: "writer_id".into(),
            from: Some("CASCADE".into()),
            to: Some("SET NULL".into()),
        };
        let before = snap("author_id", "CASCADE");
        for dialect in [
            &crate::sql::Postgres as &dyn crate::sql::Dialect,
            &crate::sql::MySql,
        ] {
            let out = super::render_changes_between(
                std::slice::from_ref(&rename),
                &before,
                &snap("writer_id", "CASCADE"),
                dialect,
            )
            .unwrap();
            assert!(out[0].contains("book_author_id_fkey"), "{out:?}");
            assert!(out[2].contains("ADD CONSTRAINT") && out[2].contains("book_writer_id_fkey"));
            assert_eq!(out.len(), 3, "{out:?}");
            let both = [rename.clone(), on_delete.clone()];
            let out = super::render_changes_between(
                &both,
                &before,
                &snap("writer_id", "SET NULL"),
                dialect,
            )
            .unwrap();
            let adds = out.iter().filter(|s| s.contains("ADD CONSTRAINT")).count();
            assert_eq!(adds, 1, "{out:?}");
            // The ON DELETE op drops the FK under the name it still has.
            assert!(out[1].contains("DROP") && out[1].contains("book_author_id_fkey"));
            // Renamed again after: each op drops the name the FK has then.
            let chain = [
                rename.clone(),
                on_delete.clone(),
                SchemaChange::RenameColumn {
                    table: "book".into(),
                    old_column: "writer_id".into(),
                    new_column: "owner_id".into(),
                },
            ];
            let out = super::render_changes_between(
                &chain,
                &before,
                &snap("owner_id", "SET NULL"),
                dialect,
            )
            .unwrap();
            let fk_ops: Vec<&str> = out
                .iter()
                .filter_map(|s| {
                    let op = if s.contains("ADD CONSTRAINT") {
                        "add"
                    } else if s.contains("DROP CONSTRAINT") || s.contains("DROP FOREIGN KEY") {
                        "drop"
                    } else {
                        return None;
                    };
                    ["author_id", "writer_id", "owner_id"]
                        .into_iter()
                        .find(|c| s.contains(&format!("book_{c}_fkey")))
                        .map(|_| op)
                })
                .collect();
            assert_eq!(fk_ops, ["drop", "add", "drop", "add"], "{out:?}");
            let named = |i: usize, c: &str| {
                out.iter()
                    .filter(|s| s.contains("CONSTRAINT") || s.contains("FOREIGN KEY"))
                    .nth(i)
                    .is_some_and(|s| s.contains(&format!("book_{c}_fkey")))
            };
            assert!(named(0, "author_id") && named(1, "writer_id"), "{out:?}");
            assert!(named(2, "writer_id") && named(3, "owner_id"), "{out:?}");
        }
    }
}
