//! The v2 ledger (6.2) and its events table. The ledger tables are v2
//! tables themselves: their DDL comes from the renderer, so the column
//! types follow the one type mapping.

use super::render::{create_table_sql_for, Unit};
use super::state::{ColumnDef, TableState, Target};
use crate::core::SqlValue;
use crate::migrate::MigrateError;
use crate::sql::{raw_execute_pool, raw_query_pool, Pool};

/// Where a migration stands in the ledger. Pending = no row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowState {
    Running,
    Applied,
    Failed,
    RollingBack,
}

impl RowState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::RollingBack => "rolling_back",
        }
    }

    fn parse(s: &str) -> Result<Self, MigrateError> {
        Ok(match s {
            "running" | "partial" => Self::Running,
            "applied" => Self::Applied,
            "failed" => Self::Failed,
            "rolling_back" => Self::RollingBack,
            other => {
                return Err(MigrateError::Validation(format!(
                    "the v2 ledger holds an unknown state `{other}`"
                )))
            }
        })
    }
}

/// One ledger row, as the runner reads it.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) app: String,
    pub(crate) id: String,
    pub(crate) checksum: String,
    pub(crate) applied_body: String,
    pub(crate) state: RowState,
    pub(crate) unit_plan: Vec<Unit>,
    pub(crate) pos: i64,
    pub(crate) started_unit: Option<i64>,
    pub(crate) run_id: String,
    /// Why the row failed (class only); `stopped` for a planned stop.
    pub(crate) error_class: Option<String>,
    /// The DDL session of `started_unit`, reaped before its probe (6.5).
    pub(crate) session: Option<super::session::Session>,
}

/// A ledger row's key, checksum and state ([`Ledger::applied`]).
#[cfg(feature = "tenancy")]
#[derive(Debug, Clone)]
pub(crate) struct Brief {
    pub(crate) app: String,
    pub(crate) id: String,
    pub(crate) checksum: String,
    pub(crate) state: RowState,
}

/// Which way a batched unit walks its key range (6.2 `direction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Walk {
    Up,
    Down,
}

/// The ledger's table names, from the configured base.
#[derive(Debug, Clone)]
pub(crate) struct Ledger {
    pub(crate) table: String,
    pub(crate) events: String,
    /// Batch progress per `(app, id, op_index)` (6.2).
    pub(crate) progress: String,
    /// Undo rows of data steps that capture them (7.4).
    pub(crate) undo: String,
}

impl Ledger {
    /// The default ledger: `__rustango_migrations_v2` and `_v2_events`.
    pub(crate) fn default_names() -> Self {
        Self::for_base(crate::migrate::LEDGER_TABLE)
    }

    /// The v2 ledger of the v1 ledger name `base`: one per app sharing a database.
    pub(crate) fn for_base(base: &str) -> Self {
        let table = crate::migrate::v2_ledger(base);
        Self {
            events: format!("{table}_events"),
            progress: format!("{table}_progress"),
            undo: format!("{table}_undo"),
            table,
        }
    }

    /// The ledger `opts` names, else the default.
    /// The framework system chain's ledger (design item 23), one per database.
    pub(crate) fn system() -> Self {
        let table = SYSTEM_LEDGER.to_owned();
        Self {
            events: format!("{table}_events"),
            progress: format!("{table}_progress"),
            undo: format!("{table}_undo"),
            table,
        }
    }

    pub(crate) fn of(opts: &super::run::Options) -> Self {
        opts.ledger
            .as_deref()
            .map_or_else(Self::default_names, Self::for_base)
    }
}

/// The system chain's v2 ledger table.
pub(crate) const SYSTEM_LEDGER: &str = "__rustango_system_v2";

fn col(name: &str, ty: &str, nullable: bool, max_length: Option<u32>) -> ColumnDef {
    ColumnDef {
        portable_default: None,
        column: name.into(),
        ty: ty.into(),
        nullable,
        max_length,
        primary_key: false,
        auto: false,
        default: None,
        generated_as: None,
        comment: None,
        case_insensitive: false,
        sealed: false,
    }
}

fn table(name: &str, columns: Vec<ColumnDef>) -> TableState {
    TableState {
        name: name.into(),
        scope: Target::Tenant,
        columns,
        indexes: Vec::new(),
        checks: Vec::new(),
        fks: Vec::new(),
        comment: None,
        pg_pkey: None,
        pg_sequence: None,
    }
}

fn ledger_table(name: &str) -> TableState {
    let mut app = col("app", "string", false, Some(100));
    app.primary_key = true;
    let mut id = col("id", "string", false, Some(200));
    id.primary_key = true;
    let mut adopted = col("adopted_from_legacy", "bool", false, None);
    adopted.default = Some("FALSE".into());
    table(
        name,
        vec![
            app,
            id,
            col("seq", "i64", false, None),
            col("checksum", "string", false, Some(64)),
            col("checksum_alg", "i32", false, None),
            col("applied_body", "string", false, None),
            col("state", "string", false, Some(16)),
            col("unit_plan", "string", false, None),
            col("unit_plan_hash", "string", false, Some(64)),
            col("engine_version", "string", false, Some(32)),
            col("pos", "i64", false, None),
            col("started_unit", "i64", true, None),
            col("session", "string", true, None),
            col("run_id", "string", false, Some(64)),
            col("heartbeat_at", "datetime", true, None),
            col("started_at", "datetime", true, None),
            col("finished_at", "datetime", true, None),
            col("state_before_hash", "string", true, Some(64)),
            col("state_after_hash", "string", true, Some(64)),
            col("step_hashes", "string", true, None),
            col("error_class", "string", true, None),
            col("undo_pruned_at", "datetime", true, None),
            adopted,
        ],
    )
}

fn progress_table(name: &str) -> TableState {
    let mut app = col("app", "string", false, Some(100));
    app.primary_key = true;
    let mut id = col("id", "string", false, Some(200));
    id.primary_key = true;
    let mut op = col("op_index", "i32", false, None);
    op.primary_key = true;
    table(
        name,
        vec![
            app,
            id,
            op,
            // The cursor as JSON, typed by the key's `ty`.
            col("effect_hi", "string", true, None),
            col("done_hi", "string", true, None),
            col("rows_done", "i64", false, None),
            col("batches", "i64", false, None),
            col("direction", "string", false, Some(8)),
            col("overrides", "string", true, None),
            col("updated_at", "datetime", true, None),
        ],
    )
}

fn undo_table(name: &str) -> TableState {
    let mut id = col("id", "i64", false, None);
    id.primary_key = true;
    id.auto = true;
    table(
        name,
        vec![
            id,
            col("app", "string", false, Some(100)),
            col("mig", "string", false, Some(200)),
            col("op_index", "i32", false, None),
            // insert | update | delete
            col("kind", "string", false, Some(8)),
            col("tbl", "string", false, Some(63)),
            col("key_col", "string", false, Some(63)),
            // The key as JSON, decoded by the key column's type.
            col("key_json", "string", false, None),
            col("key_hash", "string", false, Some(64)),
            col("col", "string", true, Some(63)),
            col("old_json", "string", true, None),
            // The written value's canonical hash: the restore skips a row
            // whose value no longer matches (the user edited it).
            col("new_hash", "string", true, Some(64)),
            col("row_json", "string", true, None),
            col("at", "datetime", true, None),
        ],
    )
}

/// The registry's cache of each tenant's migrate state (8.3): one row per
/// tenant, the error class only, never driver text or row values.
#[cfg(feature = "tenancy")]
pub(crate) const TENANT_CACHE: &str = "__rustango_tenant_migrations__";

#[cfg(feature = "tenancy")]
fn tenant_cache_table() -> TableState {
    let mut slug = col("slug", "string", false, Some(200));
    slug.primary_key = true;
    table(
        TENANT_CACHE,
        vec![
            slug,
            // running | applied | failed
            col("state", "string", false, Some(16)),
            col("head", "string", true, Some(200)),
            col("error_class", "string", true, Some(64)),
            col("run_id", "string", true, Some(64)),
            col("updated_at", "datetime", true, None),
        ],
    )
}

/// One tenant's cached state: `(slug, state, head, error_class)`.
#[cfg(feature = "tenancy")]
pub(crate) type CachedTenant = (String, String, Option<String>, Option<String>);

/// Create the registry cache; once, before tenants run at once.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "tenancy")]
pub(crate) async fn cache_ensure(registry: &Pool) -> Result<(), MigrateError> {
    if !crate::migrate::try_table_exists_here(registry, TENANT_CACHE).await? {
        raw_execute_pool(
            registry,
            &create_table_sql_for(registry.dialect(), &tenant_cache_table()),
            Vec::new(),
        )
        .await?;
    }
    Ok(())
}

/// Write `slug`'s cached state, creating the cache on first use.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "tenancy")]
pub(crate) async fn cache_put(
    registry: &Pool,
    slug: &str,
    state: &str,
    head: Option<&str>,
    error_class: Option<&str>,
    run_id: &str,
) -> Result<(), MigrateError> {
    cache_ensure(registry).await?;
    let d = registry.dialect();
    let t = d.quote_ident(TENANT_CACHE);
    raw_execute_pool(
        registry,
        &format!("DELETE FROM {t} WHERE slug = {}", d.placeholder(1)),
        vec![s(slug)],
    )
    .await?;
    let opt = |v: Option<&str>| v.map_or(SqlValue::Null, s);
    raw_execute_pool(
        registry,
        &format!(
            "INSERT INTO {t} (slug, state, head, error_class, run_id, updated_at) \
             VALUES ({}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.utc_now_sql()
        ),
        vec![s(slug), s(state), opt(head), opt(error_class), s(run_id)],
    )
    .await?;
    Ok(())
}

/// Every cached tenant; none before the first fleet run.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "tenancy")]
pub(crate) async fn cache_all(registry: &Pool) -> Result<Vec<CachedTenant>, MigrateError> {
    if !crate::migrate::try_table_exists_here(registry, TENANT_CACHE).await? {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT slug, state, head, error_class FROM {} ORDER BY slug",
        registry.dialect().quote_ident(TENANT_CACHE)
    );
    Ok(raw_query_pool(&sql, Vec::new(), registry).await?)
}

/// The registry's fleet run record (8.3): one row per fleet run.
#[cfg(feature = "tenancy")]
pub(crate) const FLEET_RUNS: &str = "__rustango_fleet_runs__";

#[cfg(feature = "tenancy")]
fn fleet_runs_table() -> TableState {
    let mut id = col("run_id", "string", false, Some(64));
    id.primary_key = true;
    table(
        FLEET_RUNS,
        vec![
            id,
            // up | down
            col("direction", "string", false, Some(8)),
            col("plan_hash", "string", false, Some(64)),
            // running | succeeded | failed | abandoned | resumed
            col("state", "string", false, Some(16)),
            col("heartbeat_at", "datetime", true, None),
            col("started_at", "datetime", true, None),
            col("finished_at", "datetime", true, None),
        ],
    )
}

/// Open a fleet run record. Called under the fleet lock, so a `running`
/// row is a run that died: the same plan and direction resumes it,
/// another one refuses unless `abandon`.
///
/// # Errors
/// Another plan's unfinished run, or driver failures.
#[cfg(feature = "tenancy")]
pub(crate) async fn fleet_open(
    registry: &Pool,
    run_id: &str,
    direction: &str,
    plan_hash: &str,
    abandon: bool,
) -> Result<(), MigrateError> {
    let d = registry.dialect();
    if !crate::migrate::try_table_exists_here(registry, FLEET_RUNS).await? {
        raw_execute_pool(
            registry,
            &create_table_sql_for(d, &fleet_runs_table()),
            Vec::new(),
        )
        .await?;
    }
    let t = d.quote_ident(FLEET_RUNS);
    let open: Vec<(String, String, String)> = raw_query_pool(
        &format!(
            "SELECT run_id, direction, plan_hash FROM {t} WHERE state = {}",
            p(registry, 1)
        ),
        vec![s("running")],
        registry,
    )
    .await?;
    for (id, dir, plan) in open {
        let same = dir == direction && plan == plan_hash;
        if !same && !abandon {
            return Err(MigrateError::Validation(format!(
                "fleet run {id} ({dir}, plan {}) did not finish and this one has another plan: \
                 finish it with the release that started it, or pass --abandon-fleet-run",
                &plan[..plan.len().min(12)]
            )));
        }
        let to = if same { "resumed" } else { "abandoned" };
        tracing::warn!(target: "rustango::migrate", run = %id, "unfinished fleet run {to}");
        fleet_set(registry, &id, to).await?;
    }
    raw_execute_pool(
        registry,
        &format!(
            "INSERT INTO {t} (run_id, direction, plan_hash, state, heartbeat_at, started_at) \
             VALUES ({}, {}, {}, {}, {now}, {now})",
            p(registry, 1),
            p(registry, 2),
            p(registry, 3),
            p(registry, 4),
            now = d.utc_now_sql()
        ),
        vec![s(run_id), s(direction), s(plan_hash), s("running")],
    )
    .await?;
    Ok(())
}

/// Refresh a fleet run's heartbeat.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "tenancy")]
pub(crate) async fn fleet_beat(registry: &Pool, run_id: &str) -> Result<(), MigrateError> {
    let d = registry.dialect();
    raw_execute_pool(
        registry,
        &format!(
            "UPDATE {} SET heartbeat_at = {} WHERE run_id = {}",
            d.quote_ident(FLEET_RUNS),
            d.utc_now_sql(),
            p(registry, 1)
        ),
        vec![s(run_id)],
    )
    .await?;
    Ok(())
}

/// End a fleet run: `succeeded` or `failed`.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "tenancy")]
pub(crate) async fn fleet_close(
    registry: &Pool,
    run_id: &str,
    state: &str,
) -> Result<(), MigrateError> {
    fleet_set(registry, run_id, state).await
}

#[cfg(feature = "tenancy")]
async fn fleet_set(registry: &Pool, run_id: &str, state: &str) -> Result<(), MigrateError> {
    let d = registry.dialect();
    raw_execute_pool(
        registry,
        &format!(
            "UPDATE {} SET state = {}, finished_at = {} WHERE run_id = {}",
            d.quote_ident(FLEET_RUNS),
            p(registry, 1),
            d.utc_now_sql(),
            p(registry, 2)
        ),
        vec![s(state), s(run_id)],
    )
    .await?;
    Ok(())
}

/// Every fleet run record, oldest first: `(run_id, direction, state)`.
///
/// # Errors
/// Driver failures.
#[cfg(all(test, feature = "tenancy", feature = "postgres"))]
pub(crate) async fn fleet_runs(
    registry: &Pool,
) -> Result<Vec<(String, String, String)>, MigrateError> {
    if !crate::migrate::try_table_exists_here(registry, FLEET_RUNS).await? {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT run_id, direction, state FROM {} ORDER BY started_at, run_id",
        registry.dialect().quote_ident(FLEET_RUNS)
    );
    Ok(raw_query_pool(&sql, Vec::new(), registry).await?)
}

/// One captured change, as the restore reads it.
#[derive(Debug, Clone)]
pub(crate) struct UndoRow {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) table: String,
    pub(crate) key_col: String,
    pub(crate) key_json: String,
    pub(crate) col: Option<String>,
    pub(crate) old_json: Option<String>,
    pub(crate) new_hash: Option<String>,
    pub(crate) row_json: Option<String>,
}

fn events_table(name: &str) -> TableState {
    let mut id = col("id", "i64", false, None);
    id.primary_key = true;
    id.auto = true;
    table(
        name,
        vec![
            id,
            col("kind", "string", false, Some(32)),
            col("app", "string", false, Some(100)),
            col("mig", "string", false, Some(200)),
            col("checksum", "string", true, Some(64)),
            col("run_id", "string", false, Some(64)),
            col("os_user", "string", true, Some(100)),
            col("host", "string", true, Some(200)),
            col("app_version", "string", true, Some(32)),
            col("reason", "string", true, None),
            col("at", "datetime", true, None),
        ],
    )
}

/// MySQL `GRANTEE` (`'user'@'host'`) names the login bound at `n`.
fn grantee_is(pool: &Pool, n: usize) -> String {
    format!(
        "SUBSTRING(GRANTEE, 2, LOCATE('''@', GRANTEE) - 2) = {}",
        p(pool, n)
    )
}

/// Write privileges MySQL `login` holds on the whole database or server:
/// no table-level REVOKE narrows them, so the ledger cannot be locked away
/// from it. The refusal names the fix; `None` elsewhere.
///
/// # Errors
/// Driver failures (reading grants needs `SELECT` on the `mysql` schema).
#[cfg(feature = "tenancy")]
pub(crate) async fn wide_grants(pool: &Pool, login: &str) -> Result<Option<String>, MigrateError> {
    const WRITES: &str = "('INSERT', 'UPDATE', 'DELETE', 'DROP', 'ALTER', 'CREATE')";
    if pool.dialect().kind() != crate::sql::DialectKind::Mysql {
        return Ok(None);
    }
    let sql = format!(
        "SELECT PRIVILEGE_TYPE FROM information_schema.SCHEMA_PRIVILEGES \
         WHERE TABLE_SCHEMA = DATABASE() AND PRIVILEGE_TYPE IN {WRITES} AND {} \
         UNION SELECT PRIVILEGE_TYPE FROM information_schema.USER_PRIVILEGES \
         WHERE PRIVILEGE_TYPE IN {WRITES} AND {}",
        grantee_is(pool, 1),
        grantee_is(pool, 2)
    );
    let found: Vec<(String,)> = raw_query_pool(&sql, vec![s(login), s(login)], pool).await?;
    if found.is_empty() {
        return Ok(None);
    }
    let privs: Vec<String> = found.into_iter().map(|r| r.0).collect();
    Ok(Some(format!(
        "tenant login `{login}` holds database-level {} on its database, which MySQL cannot \
         narrow by table: grant it table-level privileges instead (or a separate login with \
         no database-wide writes), so the ledger stays read-only to it",
        privs.join(", ")
    )))
}

fn p(pool: &Pool, n: usize) -> String {
    pool.dialect().placeholder(n)
}

fn s(v: impl Into<String>) -> SqlValue {
    SqlValue::String(v.into())
}

impl Ledger {
    /// The ledger table's name, quoted: a configured base may need it.
    fn qt(&self, pool: &Pool) -> String {
        pool.dialect().quote_ident(&self.table)
    }

    /// The events table's name, quoted.
    fn qe(&self, pool: &Pool) -> String {
        pool.dialect().quote_ident(&self.events)
    }

    /// Create the ledger and events tables if absent (the caller holds the lock).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn ensure(&self, pool: &Pool) -> Result<(), MigrateError> {
        let d = pool.dialect();
        for t in [
            ledger_table(&self.table),
            events_table(&self.events),
            progress_table(&self.progress),
            undo_table(&self.undo),
        ] {
            if !crate::migrate::try_table_exists_here(pool, &t.name).await? {
                raw_execute_pool(pool, &create_table_sql_for(d, &t), Vec::new()).await?;
                // The restore finds a key's captured changes by its hash (7.4).
                if t.name == self.undo {
                    let ix = format!(
                        "CREATE INDEX {} ON {} (key_hash)",
                        d.quote_ident(&format!("{}_kh", self.undo)),
                        d.quote_ident(&self.undo)
                    );
                    raw_execute_pool(pool, &ix, Vec::new()).await?;
                }
            }
        }
        Ok(())
    }

    /// The tenant login gets no write access to the ledger tables, only
    /// SELECT on the ledger for boot status (8.1). PG roles; a no-op elsewhere.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn revoke(&self, pool: &Pool, role: &str) -> Result<(), MigrateError> {
        match pool.dialect().kind() {
            crate::sql::DialectKind::Postgres => self.revoke_pg(pool, role).await,
            crate::sql::DialectKind::Mysql => self.revoke_mysql(pool, role).await,
            _ => Ok(()),
        }
    }

    async fn revoke_pg(&self, pool: &Pool, role: &str) -> Result<(), MigrateError> {
        #[cfg(feature = "postgres")]
        if let Pool::Postgres(pg) = pool {
            let mut conn = pg.acquire().await?;
            return Self::pg_lock_down(&[self], &mut conn, None, role).await;
        }
        let _ = (pool, role);
        Ok(())
    }

    /// PG: `role` keeps only SELECT on the tables of `ledgers` that exist
    /// (in `schema`, or on the path), and the revoke is checked: an owner or
    /// a right through another role would keep writing, so it refuses.
    ///
    /// # Errors
    /// Driver failures, or `role` can still write a ledger table.
    #[cfg(feature = "postgres")]
    pub(crate) async fn pg_lock_down(
        ledgers: &[&Self],
        conn: &mut sqlx::PgConnection,
        schema: Option<&str>,
        role: &str,
    ) -> Result<(), MigrateError> {
        let q = |t: &str| crate::sql::quote_with(t, '"');
        let name = |t: &str| schema.map_or_else(|| q(t), |s| format!("{}.{}", q(s), q(t)));
        let all: Vec<String> = ledgers
            .iter()
            .flat_map(|l| [&l.table, &l.events, &l.progress, &l.undo])
            .map(|t| name(t))
            .collect();
        let readable: Vec<String> = ledgers.iter().map(|l| name(&l.table)).collect();
        let present: Vec<String> = sqlx::query_scalar(
            "SELECT t FROM unnest($1::text[]) t WHERE to_regclass(t) IS NOT NULL",
        )
        .bind(&all)
        .fetch_all(&mut *conn)
        .await?;
        if present.is_empty() {
            return Ok(());
        }
        let r = q(role);
        let mut stmts = vec![format!(
            "REVOKE ALL ON TABLE {} FROM {r}",
            present.join(", ")
        )];
        let read: Vec<&String> = readable.iter().filter(|t| present.contains(t)).collect();
        if !read.is_empty() {
            let list: Vec<&str> = read.iter().map(|t| t.as_str()).collect();
            stmts.push(format!("GRANT SELECT ON TABLE {} TO {r}", list.join(", ")));
        }
        for sql in &stmts {
            sqlx::query(sql).execute(&mut *conn).await?;
        }
        let writable: Vec<String> = sqlx::query_scalar(
            "SELECT t FROM unnest($1::text[]) t \
             WHERE has_table_privilege($2, t, 'INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER')",
        )
        .bind(&present)
        .bind(role)
        .fetch_all(&mut *conn)
        .await?;
        if writable.is_empty() {
            return Ok(());
        }
        Err(MigrateError::Validation(format!(
            "tenant login `{role}` can still write {} after the revoke: it owns them or holds \
             rights through another role; make the migrator their owner (`ALTER TABLE .. OWNER \
             TO <migrator>`) and drop the other grants, then migrate again",
            writable.join(", ")
        )))
    }

    /// MySQL cannot narrow a database-level grant by table (design item
    /// 18): `login`'s table-level grants on the ledger tables go, SELECT on
    /// the ledger stays. [`wide_grants`] refuses the rest beforehand.
    async fn revoke_mysql(&self, pool: &Pool, login: &str) -> Result<(), MigrateError> {
        let d = pool.dialect();
        for t in [&self.table, &self.events, &self.progress, &self.undo] {
            let grants: Vec<(String, String)> = raw_query_pool(
                &format!(
                    "SELECT GRANTEE, PRIVILEGE_TYPE FROM information_schema.TABLE_PRIVILEGES \
                     WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = {} AND {}",
                    p(pool, 1),
                    grantee_is(pool, 2)
                ),
                vec![s(t.as_str()), s(login)],
                pool,
            )
            .await?;
            let mut by_grantee: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for (grantee, privilege) in grants {
                if !(t == &self.table && privilege == "SELECT") {
                    by_grantee.entry(grantee).or_default().push(privilege);
                }
            }
            for (grantee, privs) in by_grantee {
                // The grantee is the catalog's own quoted `'user'@'host'`.
                raw_execute_pool(
                    pool,
                    &format!(
                        "REVOKE {} ON {} FROM {grantee}",
                        privs.join(", "),
                        d.quote_ident(t)
                    ),
                    Vec::new(),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Every row's key, checksum and state, without bodies or plans.
    ///
    /// # Errors
    /// Driver failures, or a state that does not decode.
    #[cfg(feature = "tenancy")]
    pub(crate) async fn applied(&self, pool: &Pool) -> Result<Vec<Brief>, MigrateError> {
        if !crate::migrate::try_table_exists_here(pool, &self.table).await? {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT app, id, checksum, state FROM {} ORDER BY seq",
            self.qt(pool)
        );
        let raw: Vec<(String, String, String, String)> =
            raw_query_pool(&sql, Vec::new(), pool).await?;
        raw.into_iter()
            .map(|(app, id, checksum, state)| {
                Ok(Brief {
                    state: RowState::parse(&state)?,
                    app,
                    id,
                    checksum,
                })
            })
            .collect()
    }

    /// Every row.
    ///
    /// # Errors
    /// Driver failures, or a row that does not decode.
    pub(crate) async fn rows(&self, pool: &Pool) -> Result<Vec<Row>, MigrateError> {
        if !crate::migrate::try_table_exists_here(pool, &self.table).await? {
            return Ok(Vec::new());
        }
        type Raw = (
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            Option<i64>,
            String,
            Option<String>,
            Option<String>,
        );
        let sql = format!(
            "SELECT app, id, checksum, applied_body, state, unit_plan, pos, started_unit, run_id, \
             error_class, session FROM {} ORDER BY seq",
            self.qt(pool)
        );
        let raw: Vec<Raw> = raw_query_pool(&sql, Vec::new(), pool).await?;
        raw.into_iter()
            .map(
                |(
                    app,
                    id,
                    checksum,
                    applied_body,
                    state,
                    plan,
                    pos,
                    started_unit,
                    run_id,
                    error_class,
                    session,
                )| {
                    Ok(Row {
                        unit_plan: serde_json::from_str(&plan)?,
                        state: RowState::parse(&state)?,
                        app,
                        id,
                        checksum,
                        applied_body,
                        pos,
                        started_unit,
                        run_id,
                        error_class,
                        session: session.as_deref().map(serde_json::from_str).transpose()?,
                    })
                },
            )
            .collect()
    }

    /// Insert a `running` row (or `rolling_back` for a down plan's resume
    /// marker), position before the first unit.
    ///
    /// # Errors
    /// Driver failures.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn insert(
        &self,
        pool: &Pool,
        key: (&str, &str),
        checksum: &str,
        body: &str,
        state: RowState,
        plan: &[Unit],
        run_id: &str,
        hashes: (&str, Option<&str>),
        adopted: bool,
    ) -> Result<(), MigrateError> {
        let next: Vec<(i64,)> = raw_query_pool(
            &format!("SELECT COALESCE(MAX(seq), 0) FROM {}", self.qt(pool)),
            Vec::new(),
            pool,
        )
        .await?;
        let seq = next.first().map_or(0, |r| r.0) + 1;
        let plan_json = serde_json::to_string(plan)?;
        let plan_hash = super::checksum::hex(&<sha2::Sha256 as sha2::Digest>::digest(
            plan_json.as_bytes(),
        ));
        let now = pool.dialect().utc_now_sql();
        let sql = format!(
            "INSERT INTO {} (app, id, seq, checksum, checksum_alg, applied_body, state, unit_plan, \
             unit_plan_hash, engine_version, pos, run_id, heartbeat_at, started_at, \
             state_before_hash, state_after_hash, adopted_from_legacy) VALUES ({}, {}, {}, {}, {}, \
             {}, {}, {}, {}, {}, -1, {}, {now}, {now}, {}, {}, {})",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
            p(pool, 7),
            p(pool, 8),
            p(pool, 9),
            p(pool, 10),
            p(pool, 11),
            p(pool, 12),
            p(pool, 13),
            p(pool, 14),
        );
        raw_execute_pool(
            pool,
            &sql,
            vec![
                s(key.0),
                s(key.1),
                SqlValue::I64(seq),
                s(checksum),
                SqlValue::I32(super::checksum::CHECKSUM_ALG as i32),
                s(body),
                s(state.as_str()),
                s(plan_json),
                s(plan_hash),
                s(env!("CARGO_PKG_VERSION")),
                s(run_id),
                s(hashes.0),
                hashes.1.map_or(SqlValue::Null, s),
                SqlValue::Bool(adopted),
            ],
        )
        .await?;
        Ok(())
    }

    /// Take over a crashed or failed run's row: fenced on its old `run_id`.
    /// A failed row runs again (`running`, its old error class gone), so its
    /// heartbeat counts; a rolling-back row stays rolling back.
    ///
    /// # Errors
    /// Driver failures, or the row moved meanwhile.
    pub(crate) async fn claim(
        &self,
        pool: &Pool,
        key: (&str, &str),
        old: &str,
        new: &str,
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET run_id = {}, heartbeat_at = {}, error_class = NULL, \
             state = CASE WHEN state = 'rolling_back' THEN state ELSE 'running' END \
             WHERE app = {} AND id = {} AND run_id = {}",
            self.qt(pool),
            p(pool, 1),
            pool.dialect().utc_now_sql(),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
        );
        let n = raw_execute_pool(pool, &sql, vec![s(new), s(key.0), s(key.1), s(old)]).await?;
        fenced(n)
    }

    /// The fenced position update, with its binds: `pos = to` where the row
    /// is ours and still at `from`. Run inside a unit's transaction, or alone.
    pub(crate) fn advance(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        from: i64,
        to: i64,
    ) -> (String, Vec<SqlValue>) {
        let sql = format!(
            "UPDATE {} SET pos = {}, started_unit = NULL, session = NULL, heartbeat_at = {} \
             WHERE app = {} AND id = {} AND run_id = {} AND pos = {}",
            self.qt(pool),
            p(pool, 1),
            pool.dialect().utc_now_sql(),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
        );
        (
            sql,
            vec![
                SqlValue::I64(to),
                s(key.0),
                s(key.1),
                s(run_id),
                SqlValue::I64(from),
            ],
        )
    }

    /// Refresh the heartbeat of every row this run holds.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn touch(&self, pool: &Pool, run_id: &str) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET heartbeat_at = {} WHERE run_id = {} AND state IN ({}, {})",
            self.qt(pool),
            pool.dialect().utc_now_sql(),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
        );
        raw_execute_pool(
            pool,
            &sql,
            vec![
                s(run_id),
                s(RowState::Running.as_str()),
                s(RowState::RollingBack.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    /// A `running` row whose heartbeat is older than `stale`, by the DB clock:
    /// `(app, id, age in seconds)`.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn stale_running(
        &self,
        pool: &Pool,
        stale: std::time::Duration,
    ) -> Result<Option<(String, String, i64)>, MigrateError> {
        let age = match pool.dialect().kind() {
            crate::sql::DialectKind::Postgres => {
                "CAST(EXTRACT(EPOCH FROM (now() - heartbeat_at)) AS BIGINT)"
            }
            crate::sql::DialectKind::Mysql => {
                "TIMESTAMPDIFF(SECOND, heartbeat_at, UTC_TIMESTAMP(6))"
            }
            _ => "CAST((julianday('now') - julianday(heartbeat_at)) * 86400 AS INTEGER)",
        };
        let sql = format!(
            "SELECT app, id, {age} FROM {} WHERE state = {} AND heartbeat_at IS NOT NULL",
            self.qt(pool),
            p(pool, 1),
        );
        let rows: Vec<(String, String, i64)> =
            raw_query_pool(&sql, vec![s(RowState::Running.as_str())], pool).await?;
        let limit = i64::try_from(stale.as_secs()).unwrap_or(i64::MAX);
        Ok(rows.into_iter().find(|r| r.2 >= limit))
    }

    /// Mark autocommit unit `unit` as started (fenced on `pos = unit - 1`).
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn start_unit(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        unit: i64,
        session: Option<&super::session::Session>,
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET started_unit = {}, session = {}, heartbeat_at = {} \
             WHERE app = {} AND id = {} AND run_id = {} AND pos = {}",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            pool.dialect().utc_now_sql(),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
        );
        let session = match session {
            Some(x) => s(&serde_json::to_string(x)?),
            None => SqlValue::Null,
        };
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                SqlValue::I64(unit),
                session,
                s(key.0),
                s(key.1),
                s(run_id),
                SqlValue::I64(unit - 1),
            ],
        )
        .await?;
        fenced(n)
    }

    /// Set the row's state (and, when given, its after-hash), fenced.
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn set_state(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        state: RowState,
        error_class: Option<&str>,
    ) -> Result<(), MigrateError> {
        let finished = if state == RowState::Applied {
            format!(", finished_at = {}", pool.dialect().utc_now_sql())
        } else {
            String::new()
        };
        let sql = format!(
            "UPDATE {} SET state = {}, error_class = {}{finished} WHERE app = {} AND id = {} AND run_id = {}",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
        );
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                s(state.as_str()),
                error_class.map_or(SqlValue::Null, s),
                s(key.0),
                s(key.1),
                s(run_id),
            ],
        )
        .await?;
        fenced(n)
    }

    /// Replace a row's plan for a rollback: `rolling_back`, position reset.
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn begin_rollback(
        &self,
        pool: &Pool,
        key: (&str, &str),
        old_run: &str,
        run_id: &str,
        plan: &[Unit],
    ) -> Result<(), MigrateError> {
        self.replan(
            pool,
            key,
            (old_run, run_id),
            RowState::RollingBack,
            plan,
            -1,
        )
        .await
    }

    /// Take the row from `runs.0` for `runs.1` with a new plan, state and
    /// position (fenced): a rollback begins, or goes forward again.
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn replan(
        &self,
        pool: &Pool,
        key: (&str, &str),
        runs: (&str, &str),
        state: RowState,
        plan: &[Unit],
        pos: i64,
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET state = {}, unit_plan = {}, pos = {}, started_unit = NULL, \
             session = NULL, run_id = {} WHERE app = {} AND id = {} AND run_id = {}",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
            p(pool, 7),
        );
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                s(state.as_str()),
                s(serde_json::to_string(plan)?),
                SqlValue::I64(pos),
                s(runs.1),
                s(key.0),
                s(key.1),
                s(runs.0),
            ],
        )
        .await?;
        fenced(n)
    }
    /// The statements that forget a rolled-back row, in one transaction:
    /// its progress and undo rows, then the row itself (fenced on `run_id`,
    /// the third bind), then the `rolled_back` event. `(sql, binds, fenced)`.
    ///
    /// # Errors
    /// Driver failures reading which tables exist.
    pub(crate) async fn remove_stmts(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        checksum: &str,
    ) -> Result<Vec<(String, Vec<SqlValue>, bool)>, MigrateError> {
        let d = pool.dialect();
        let mut out = Vec::new();
        if crate::migrate::try_table_exists_here(pool, &self.progress).await? {
            out.push((
                format!(
                    "DELETE FROM {} WHERE app = {} AND id = {}",
                    self.qp(pool),
                    p(pool, 1),
                    p(pool, 2),
                ),
                vec![s(key.0), s(key.1)],
                false,
            ));
        }
        if crate::migrate::try_table_exists_here(pool, &self.undo).await? {
            out.push((
                format!(
                    "DELETE FROM {} WHERE app = {} AND mig = {}",
                    d.quote_ident(&self.undo),
                    p(pool, 1),
                    p(pool, 2),
                ),
                vec![s(key.0), s(key.1)],
                false,
            ));
        }
        out.push((
            format!(
                "DELETE FROM {} WHERE app = {} AND id = {} AND run_id = {}",
                self.qt(pool),
                p(pool, 1),
                p(pool, 2),
                p(pool, 3),
            ),
            vec![s(key.0), s(key.1), s(run_id)],
            true,
        ));
        let (sql, binds) = self.event_stmt(d, "rolled_back", key, Some(checksum), run_id, None);
        out.push((sql, binds, false));
        Ok(out)
    }

    /// The INSERT of one undo row; binds: app, mig, op_index, kind, tbl,
    /// key_col, key_json, key_hash, col, old_json, new_hash, row_json.
    pub(crate) fn undo_record_sql(&self, d: &dyn crate::sql::Dialect) -> String {
        let marks: Vec<String> = (1..=12).map(|n| d.placeholder(n)).collect();
        format!(
            "INSERT INTO {} (app, mig, op_index, kind, tbl, key_col, key_json, key_hash, col, \
             old_json, new_hash, row_json, at) VALUES ({}, {})",
            d.quote_ident(&self.undo),
            marks.join(", "),
            d.utc_now_sql()
        )
    }

    /// Up to `limit` undo rows of forward unit `op`, newest first.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn undo_rows_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        key: (&str, &str),
        op: usize,
        limit: u32,
    ) -> Result<Vec<UndoRow>, MigrateError> {
        let d = tx.dialect();
        let sql = format!(
            "SELECT id, kind, tbl, key_col, key_json, col, old_json, new_hash, row_json FROM {} \
             WHERE app = {} AND mig = {} AND op_index = {} ORDER BY id DESC LIMIT {limit}",
            d.quote_ident(&self.undo),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        );
        type Raw = (
            i64,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let op = i32::try_from(op).unwrap_or(i32::MAX);
        let rows: Vec<Raw> =
            crate::sql::raw_query_tx(tx, &sql, vec![s(key.0), s(key.1), SqlValue::I32(op)]).await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, kind, table, key_col, key_json, col, old_json, new_hash, row_json)| UndoRow {
                    id,
                    kind,
                    table,
                    key_col,
                    key_json,
                    col,
                    old_json,
                    new_hash,
                    row_json,
                },
            )
            .collect())
    }

    /// Delete restored undo rows, in the restore's transaction.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn undo_delete_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        ids: &[i64],
    ) -> Result<(), MigrateError> {
        if ids.is_empty() {
            return Ok(());
        }
        let d = tx.dialect();
        let marks: Vec<String> = (1..=ids.len()).map(|n| d.placeholder(n)).collect();
        let sql = format!(
            "DELETE FROM {} WHERE id IN ({})",
            d.quote_ident(&self.undo),
            marks.join(", ")
        );
        crate::sql::raw_execute_tx(tx, &sql, ids.iter().map(|i| SqlValue::I64(*i)).collect())
            .await?;
        Ok(())
    }

    fn qp(&self, pool: &Pool) -> String {
        pool.dialect().quote_ident(&self.progress)
    }

    /// A batched op's progress: `(effect_hi as JSON, rows_done, batches)`.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn progress_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        key: (&str, &str),
        op: usize,
    ) -> Result<Option<(Option<String>, i64, i64)>, MigrateError> {
        let d = tx.dialect();
        let sql = format!(
            "SELECT effect_hi, rows_done, batches FROM {} WHERE app = {} AND id = {} AND op_index = {}",
            d.quote_ident(&self.progress),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        );
        let op = i32::try_from(op).unwrap_or(i32::MAX);
        let rows: Vec<(Option<String>, i64, i64)> =
            crate::sql::raw_query_tx(tx, &sql, vec![s(key.0), s(key.1), SqlValue::I32(op)]).await?;
        Ok(rows.into_iter().next())
    }

    /// Replace a batched op's progress inside the batch's transaction.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn put_progress_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        key: (&str, &str),
        op: usize,
        effect_hi: Option<&str>,
        rows_done: i64,
        batches: i64,
        walk: Walk,
    ) -> Result<(), MigrateError> {
        let d = tx.dialect();
        let op = i32::try_from(op).unwrap_or(i32::MAX);
        if walk == Walk::Down {
            // `done_hi` keeps the forward high-water; `effect_hi` shrinks.
            let up = format!(
                "UPDATE {} SET effect_hi = {}, rows_done = {}, batches = {}, direction = {}, \
                 updated_at = {} WHERE app = {} AND id = {} AND op_index = {}",
                d.quote_ident(&self.progress),
                d.placeholder(1),
                d.placeholder(2),
                d.placeholder(3),
                d.placeholder(4),
                d.utc_now_sql(),
                d.placeholder(5),
                d.placeholder(6),
                d.placeholder(7),
            );
            crate::sql::raw_execute_tx(
                tx,
                &up,
                vec![
                    effect_hi.map_or(SqlValue::Null, s),
                    SqlValue::I64(rows_done),
                    SqlValue::I64(batches),
                    s("down"),
                    s(key.0),
                    s(key.1),
                    SqlValue::I32(op),
                ],
            )
            .await?;
            return Ok(());
        }
        let del = format!(
            "DELETE FROM {} WHERE app = {} AND id = {} AND op_index = {}",
            d.quote_ident(&self.progress),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        );
        crate::sql::raw_execute_tx(tx, &del, vec![s(key.0), s(key.1), SqlValue::I32(op)]).await?;
        let ins = format!(
            "INSERT INTO {} (app, id, op_index, effect_hi, done_hi, rows_done, batches, direction, updated_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.quote_ident(&self.progress),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.utc_now_sql(),
        );
        let hi = effect_hi.map_or(SqlValue::Null, s);
        crate::sql::raw_execute_tx(
            tx,
            &ins,
            vec![
                s(key.0),
                s(key.1),
                SqlValue::I32(op),
                hi.clone(),
                hi,
                SqlValue::I64(rows_done),
                SqlValue::I64(batches),
                s("up"),
            ],
        )
        .await?;
        Ok(())
    }

    /// The fenced heartbeat of a batch that does not finish its unit.
    pub(crate) fn heartbeat(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        pos: i64,
    ) -> (String, Vec<SqlValue>) {
        let sql = format!(
            "UPDATE {} SET heartbeat_at = {} WHERE app = {} AND id = {} AND run_id = {} AND pos = {}",
            self.qt(pool),
            pool.dialect().utc_now_sql(),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
        );
        (sql, vec![s(key.0), s(key.1), s(run_id), SqlValue::I64(pos)])
    }

    /// Append an event (the history; rows are deleted on rollback).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn event(
        &self,
        pool: &Pool,
        kind: &str,
        key: (&str, &str),
        checksum: Option<&str>,
        run_id: &str,
        reason: Option<&str>,
    ) -> Result<(), MigrateError> {
        let (sql, binds) = self.event_stmt(pool.dialect(), kind, key, checksum, run_id, reason);
        raw_execute_pool(pool, &sql, binds).await?;
        Ok(())
    }

    /// [`Self::event`] as a statement, to run in a batch's transaction.
    pub(crate) fn event_stmt(
        &self,
        d: &dyn crate::sql::Dialect,
        kind: &str,
        key: (&str, &str),
        checksum: Option<&str>,
        run_id: &str,
        reason: Option<&str>,
    ) -> (String, Vec<SqlValue>) {
        let ph = |n| d.placeholder(n);
        let sql = format!(
            "INSERT INTO {} (kind, app, mig, checksum, run_id, os_user, host, app_version, reason, at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.quote_ident(&self.events),
            ph(1),
            ph(2),
            ph(3),
            ph(4),
            ph(5),
            ph(6),
            ph(7),
            ph(8),
            ph(9),
            d.utc_now_sql(),
        );
        let env = |k: &str| std::env::var(k).ok().map_or(SqlValue::Null, s);
        (
            sql,
            vec![
                s(kind),
                s(key.0),
                s(key.1),
                checksum.map_or(SqlValue::Null, s),
                s(run_id),
                env("USER"),
                env("HOSTNAME"),
                s(env!("CARGO_PKG_VERSION")),
                reason.map_or(SqlValue::Null, s),
            ],
        )
    }

    /// The last code hash recorded per `(app, step id)`.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn step_hashes(
        &self,
        pool: &Pool,
    ) -> Result<std::collections::BTreeMap<(String, String), String>, MigrateError> {
        let mut out = std::collections::BTreeMap::new();
        if !crate::migrate::try_table_exists_here(pool, &self.events).await? {
            return Ok(out);
        }
        let sql = format!(
            "SELECT app, reason FROM {} WHERE kind = {} ORDER BY id",
            self.qe(pool),
            p(pool, 1),
        );
        let rows: Vec<(String, Option<String>)> =
            raw_query_pool(&sql, vec![s(super::run::STEP_HASH)], pool).await?;
        for (app, reason) in rows {
            if let Some((id, hash)) = reason.as_deref().and_then(|r| r.split_once(' ')) {
                out.insert((app, id.to_owned()), hash.to_owned());
            }
        }
        Ok(out)
    }

    /// The kinds of events written for `key`, oldest first (tests and status).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn event_kinds(
        &self,
        pool: &Pool,
        key: (&str, &str),
    ) -> Result<Vec<String>, MigrateError> {
        let sql = format!(
            "SELECT kind FROM {} WHERE app = {} AND mig = {} ORDER BY id",
            self.qe(pool),
            p(pool, 1),
            p(pool, 2),
        );
        let rows: Vec<(String,)> = raw_query_pool(&sql, vec![s(key.0), s(key.1)], pool).await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// The reason of the latest `kind` event of a migration.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn last_reason(
        &self,
        pool: &Pool,
        key: (&str, &str),
        kind: &str,
    ) -> Result<Option<String>, MigrateError> {
        let sql = format!(
            "SELECT reason FROM {} WHERE app = {} AND mig = {} AND kind = {} ORDER BY id DESC",
            self.qe(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
        );
        let rows: Vec<(Option<String>,)> =
            raw_query_pool(&sql, vec![s(key.0), s(key.1), s(kind)], pool).await?;
        Ok(rows.into_iter().next().and_then(|r| r.0))
    }
}

/// One row must have changed, or another run took the migration over.
pub(crate) fn fenced(n: u64) -> Result<(), MigrateError> {
    if n == 1 {
        Ok(())
    } else {
        Err(MigrateError::Validation(
            "lock lost: another run owns this migration now; nothing more was written".into(),
        ))
    }
}
