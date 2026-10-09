//! Audit log — one table that records every tracked write (insert,
//! update, delete, soft-delete) for models declared with
//! `#[rustango(audit(...))]`.
//!
//! Rows are keyed by `(entity_table, entity_pk)` instead of a per-model FK,
//! so one flat table serves any number of models with any PK shape. Query
//! `WHERE entity_table = 'post' AND entity_pk = '42'` for one row's
//! history, or drop the second clause for a per-table activity feed.
//!
//! The table is **per-tenant** in tenancy projects and per-database
//! otherwise.
//!
//! ## Source of change
//!
//! [`AuditSource`] travels in a tokio task-local, so handlers, seed
//! scripts and jobs can say who made a write without passing a context
//! object through every ORM call. The default is [`AuditSource::System`].
//! Override one call with `Model::save_on_with(conn, source)`.
//!
//! ## What gets logged
//!
//! Per-row writes record before/after values for every field named in the
//! model's `audit(track = "...")`. Bulk writes collect their entries and
//! insert them in multi-row statements sized to the bind limit.
//!
//! Rows the database removes or changes through an FK `ON DELETE`
//! action, or that PG's `truncate` (`CASCADE`) empties, are not audited
//! (planned for 0.61.0).

use serde_json::{Map, Value};

use crate::sql::sqlx;

// The PG-typed helpers below use PgRow / PgPool / Row directly. SQLite and
// MySQL go through the `*_pool` helpers further down, which dispatch per
// backend.
#[cfg(feature = "postgres")]
use crate::sql::sqlx::{postgres::PgRow, PgPool, Row};

/// Source of the change recorded in the audit log.
///
/// `System` is the default: jobs, seed scripts, framework internals.
/// `User { id }` is for request flows; admin handlers set it from the
/// session at request entry. `Custom` holds a project label such as
/// `"webhook:stripe"` or `"cli:backfill"`.
#[derive(Debug, Clone)]
pub enum AuditSource {
    System,
    User { id: String },
    Custom(String),
}

impl AuditSource {
    /// Stable string written to `audit_log.source`. The format is fixed,
    /// so a downstream index can join on it without parsing.
    #[must_use]
    pub fn as_token(&self) -> String {
        match self {
            Self::System => "system".to_owned(),
            Self::User { id } => format!("user:{id}"),
            Self::Custom(s) => s.clone(),
        }
    }
}

impl Default for AuditSource {
    fn default() -> Self {
        Self::System
    }
}

tokio::task_local! {
    /// Task-local audit source, set for the length of a request or a seed
    /// closure. Outside any scope, `current_source()` returns
    /// [`AuditSource::System`].
    pub static AUDIT_SOURCE: AuditSource;
}

tokio::task_local! {
    /// Tenant whose user the active source names; `None` when unbound.
    static SOURCE_TENANT: Option<String>;
    /// Tenant the current writes go to, where the framework knows it.
    static WRITE_TENANT: WriteScope;
    /// Pool the current audited write goes through, set by the `&Pool`
    /// write paths that emit from inside a transaction.
    static WRITE_POOL: crate::sql::PoolId;
}

/// Where writes go. With `pool` set (a `with_tenant` pass), a source
/// bound to `tenant` is kept only on writes through that pool (#2123).
#[derive(Clone)]
struct WriteScope {
    tenant: String,
    pool: Option<crate::sql::PoolId>,
}

/// How an emitter reached the database, for [`source_token`].
#[derive(Clone, Copy)]
enum Via<'a> {
    /// A caller's executor: its pool is unknown.
    Unknown,
    /// This pool.
    Pool(&'a crate::sql::Pool),
    /// The pool the enclosing `&Pool` write path set in `WRITE_POOL`.
    Scoped,
}

/// Read the active audit source. Returns [`AuditSource::System`] when no
/// [`with_source`] scope is active, and when the source belongs to one
/// tenant but the writes are not known to go to that tenant (#1229).
#[must_use]
pub fn current_source() -> AuditSource {
    let source = AUDIT_SOURCE
        .try_with(Clone::clone)
        .unwrap_or(AuditSource::System);
    match SOURCE_TENANT.try_with(Clone::clone).ok().flatten() {
        None => source,
        Some(bound)
            if WRITE_TENANT
                .try_with(|w| w.tenant == bound)
                .unwrap_or(false) =>
        {
            source
        }
        Some(_) => AuditSource::System,
    }
}

/// `source` as written through `via`. Inside a pool-bound scope the
/// tenant-bound source is kept only on that pool's writes; anywhere
/// else, a tenant B pool or the registry, it is `system` (#2123).
fn source_token(source: &AuditSource, via: Via<'_>) -> String {
    let Some(scope) = WRITE_TENANT.try_with(|w| w.pool.clone()).ok().flatten() else {
        return source.as_token();
    };
    let token = source.as_token();
    let bound = SOURCE_TENANT.try_with(|t| t.is_some()).unwrap_or(false)
        && AUDIT_SOURCE
            .try_with(|s| s.as_token() == token)
            .unwrap_or(false);
    let through_scope = match via {
        Via::Unknown => false,
        Via::Pool(pool) => scope.is(pool),
        Via::Scoped => WRITE_POOL.try_with(|p| p.same(&scope)).unwrap_or(false),
    };
    if bound && !through_scope {
        AuditSource::System.as_token()
    } else {
        token
    }
}

/// Run `fut` with its audited writes going through `pool`.
#[cfg_attr(
    not(any(feature = "admin", feature = "tenancy", feature = "forms")),
    allow(dead_code)
)]
async fn writing_via<F: std::future::Future>(pool: &crate::sql::Pool, fut: F) -> F::Output {
    WRITE_POOL.scope(crate::sql::PoolId::of(pool), fut).await
}

/// Run `fut` with `source` as the active audit source. Every audit entry
/// produced inside the future, single-row or bulk, records it.
///
/// Wrap a request handler or a seed closure with this. Outside such a
/// scope, writes record `AuditSource::System`.
pub async fn with_source<F, T>(source: AuditSource, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    AUDIT_SOURCE
        .scope(source, SOURCE_TENANT.scope(None, fut))
        .await
}

/// [`with_source`] for a tenant request: `source` names a user of the
/// tenant with slug `tenant`. Work handed off from here (a job, a
/// `for_each_tenant` pass) records it only on that tenant's writes.
#[cfg(feature = "tenancy")]
pub async fn with_tenant_source<F, T>(source: AuditSource, tenant: String, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let scope = WriteScope {
        tenant: tenant.clone(),
        pool: None,
    };
    let writes = WRITE_TENANT.scope(scope, fut);
    AUDIT_SOURCE
        .scope(source, SOURCE_TENANT.scope(Some(tenant), writes))
        .await
}

/// Run `fut` with its writes going to `tenant`, as a per-tenant sweep
/// does; with `pool`, only writes through that pool count (#2123).
#[cfg(feature = "tenancy")]
pub(crate) async fn writing_to_tenant<F, T>(
    tenant: String,
    pool: Option<&crate::sql::Pool>,
    fut: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let pool = pool.map(crate::sql::PoolId::of);
    WRITE_TENANT.scope(WriteScope { tenant, pool }, fut).await
}

/// An explicitly set source and the tenant it belongs to, for deferred
/// work. Where the writes go is not captured: the work decides that.
#[derive(Debug, Clone)]
pub(crate) struct CapturedSource {
    source: AuditSource,
    tenant: Option<String>,
}

impl CapturedSource {
    /// `None` outside any [`with_source`] scope.
    pub(crate) fn capture() -> Option<Self> {
        let source = AUDIT_SOURCE.try_with(Clone::clone).ok()?;
        let tenant = SOURCE_TENANT.try_with(Clone::clone).ok().flatten();
        Some(Self { source, tenant })
    }

    pub(crate) fn source(&self) -> &AuditSource {
        &self.source
    }

    /// The tenant the source belongs to, if bound.
    #[cfg(feature = "jobs-postgres")]
    pub(crate) fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// Rebuild one read back from a stored job row.
    #[cfg(feature = "jobs-postgres")]
    pub(crate) fn from_parts(source: AuditSource, tenant: Option<String>) -> Self {
        Self { source, tenant }
    }

    pub(crate) async fn scope<F, T>(self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        AUDIT_SOURCE
            .scope(self.source, SOURCE_TENANT.scope(self.tenant, fut))
            .await
    }
}

/// One pending audit log entry. The generated write paths build these in
/// memory, then [`emit_one`] / [`emit_many`] writes them out with, or just
/// after, the data write.
#[derive(Debug, Clone)]
pub struct PendingEntry {
    pub entity_table: &'static str,
    pub entity_pk: String,
    pub operation: AuditOp,
    pub source: AuditSource,
    pub changes: Value,
}

impl PendingEntry {
    /// An `Update` entry holding the field diff, or `None` when nothing
    /// changed, so a no-op save writes no audit row (#1907).
    #[must_use]
    pub fn update_diff(
        entity_table: &'static str,
        entity_pk: String,
        before: &[(&str, Value)],
        after: &[(&str, Value)],
    ) -> Option<Self> {
        let changes = diff_changes(before, after);
        if changes.as_object().is_some_and(Map::is_empty) {
            return None;
        }
        Some(Self {
            entity_table,
            entity_pk,
            operation: AuditOp::Update,
            source: current_source(),
            changes,
        })
    }

    /// Replace `column`'s recorded value, only if the column is tracked.
    pub fn set_tracked(&mut self, column: &str, value: Value) {
        if let Some(slot) = self.changes.get_mut(column) {
            *slot = value;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOp {
    Create,
    Update,
    Delete,
    SoftDelete,
    Restore,
    /// An operator action that is not a row write: impersonation start or
    /// end, an org config edit, a branding upload. Written by the operator
    /// console.
    Action,
}

impl AuditOp {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::SoftDelete => "soft_delete",
            Self::Restore => "restore",
            Self::Action => "action",
        }
    }
}

/// Emit a single entry against a Postgres executor. Used by per-row write
/// paths on PG. For all backends, see [`emit_one_pool`].
///
/// # `occurred_at` is bound, not defaulted
///
/// Every emit path binds it — all three dialects, single-row and batch.
/// The column default is only a backstop for hand-written SQL.
///
/// SQLite is the reason. An older database still defaults to
/// `CURRENT_TIMESTAMP`, whose `YYYY-MM-DD HH:MM:SS` spelling sorts *below*
/// the canonical `…T…` one, and SQLite cannot `ALTER TABLE` a default
/// away. A defaulted write there would store a legacy-shaped value, which
/// inverts `ORDER BY occurred_at DESC` and makes `cleanup_keep_last_n`
/// delete the newest entries. PG and MySQL bind it too, for uniformity.
///
/// # Errors
/// Driver / SQL failures from the INSERT.
#[cfg(feature = "postgres")]
pub async fn emit_one<'c, E>(executor: E, entry: &PendingEntry) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    emit_one_pg(executor, entry, Via::Unknown).await
}

#[cfg(feature = "postgres")]
async fn emit_one_pg<'c, E>(
    executor: E,
    entry: &PendingEntry,
    via: Via<'_>,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    sqlx::query(
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes", "occurred_at")
           VALUES ($1, $2, $3, $4, $5, $6)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(source_token(&entry.source, via))
    .bind(&entry.changes)
    .bind(chrono::Utc::now())
    .execute(executor)
    .await?;
    Ok(())
}

/// Emit a batch of entries on Postgres, one multi-row INSERT per
/// bind-limit-sized chunk. Several chunks run in one transaction (a
/// savepoint when `conn` is already inside one). For all backends, see
/// [`emit_many_pool`].
///
/// # Errors
/// As [`emit_one`].
#[cfg(feature = "postgres")]
pub async fn emit_many<'c, A>(conn: A, entries: &[PendingEntry]) -> Result<(), sqlx::Error>
where
    A: sqlx::Acquire<'c, Database = sqlx::Postgres>,
{
    if entries.is_empty() {
        return Ok(());
    }
    let per_insert = audit_rows_per_insert(&crate::sql::Postgres);
    if entries.len() <= per_insert {
        let mut c = conn.acquire().await?;
        return emit_chunk_pg(&mut c, entries).await;
    }
    let mut tx = conn.begin().await?;
    for chunk in entries.chunks(per_insert) {
        emit_chunk_pg(&mut tx, chunk).await?;
    }
    tx.commit().await
}

#[cfg(feature = "postgres")]
async fn emit_chunk_pg(
    conn: &mut sqlx::PgConnection,
    entries: &[PendingEntry],
) -> Result<(), sqlx::Error> {
    let stmt = audit_insert_stmt(&crate::sql::Postgres, entries, Via::Unknown)?;
    let mut q = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = crate::sql::bind_query(q, value);
    }
    q.execute(conn).await?;
    Ok(())
}

/// The audit log's table.
#[cfg(feature = "admin")]
pub(crate) const AUDIT_TABLE: &str = "rustango_audit_log";

/// Columns of an audit INSERT, in bind order.
const AUDIT_COLUMNS: [&str; 6] = [
    "entity_table",
    "entity_pk",
    "operation",
    "source",
    "changes",
    "occurred_at",
];

/// Most entries per audit INSERT. Each carries a row snapshot, so the
/// bind cap alone could pass MySQL's `max_allowed_packet` (64 MB).
const AUDIT_ROWS_PER_INSERT_MAX: usize = 100;

/// Entries per audit INSERT, so their binds fit the dialect's cap.
fn audit_rows_per_insert(dialect: &dyn crate::sql::Dialect) -> usize {
    (dialect.max_bind_params() / AUDIT_COLUMNS.len()).clamp(1, AUDIT_ROWS_PER_INSERT_MAX)
}

/// One multi-row audit INSERT, rendered by the bulk-insert writer.
fn audit_insert_stmt(
    dialect: &dyn crate::sql::Dialect,
    entries: &[PendingEntry],
    via: Via<'_>,
) -> Result<crate::sql::CompiledStatement, sqlx::Error> {
    use crate::core::{Model as _, SqlValue};
    let rows = entries
        .iter()
        .map(|e| {
            vec![
                SqlValue::String(e.entity_table.to_owned()),
                SqlValue::String(e.entity_pk.clone()),
                SqlValue::String(e.operation.as_str().to_owned()),
                SqlValue::String(source_token(&e.source, via)),
                SqlValue::Json(e.changes.clone()),
                // Stamped per row, as `emit_one` does.
                SqlValue::DateTime(chrono::Utc::now()),
            ]
        })
        .collect();
    let query = crate::core::BulkInsertQuery::new(AuditLog::SCHEMA, AUDIT_COLUMNS.to_vec(), rows);
    dialect
        .compile_bulk_insert(&query)
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))
}

/// Build a `{ "field": { "before": <v>, "after": <v> } }` JSON object from
/// two slices of `(field_name, json_value)` pairs. Fields whose value did
/// not change are left out.
#[must_use]
pub fn diff_changes(before: &[(&str, Value)], after: &[(&str, Value)]) -> Value {
    let mut out = Map::new();
    for (name, after_val) in after {
        let before_val = before
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Null);
        if &before_val != after_val {
            let mut entry = Map::new();
            entry.insert("before".into(), before_val);
            entry.insert("after".into(), after_val.clone());
            out.insert((*name).into(), Value::Object(entry));
        }
    }
    Value::Object(out)
}

/// Build a `{ "field": <after-value> }` JSON object for create,
/// soft_delete and restore, where there is no useful "before" state.
#[must_use]
pub fn snapshot_changes(after: &[(&str, Value)]) -> Value {
    let mut out = Map::new();
    for (name, val) in after {
        out.insert((*name).to_string(), val.clone());
    }
    Value::Object(out)
}

/// Render the audit-log SELECT used by [`fetch_for_entity_pool`] through
/// the dialect emitters, so no SQL here is hand-written per backend.
fn audit_select_sql(dialect: &dyn crate::sql::Dialect) -> String {
    use std::fmt::Write as _;
    let t = dialect.quote_ident("rustango_audit_log");
    let id = dialect.quote_ident("id");
    let et = dialect.quote_ident("entity_table");
    let ek = dialect.quote_ident("entity_pk");
    let op = dialect.quote_ident("operation");
    let src = dialect.quote_ident("source");
    let ch = dialect.quote_ident("changes");
    let oa = dialect.quote_ident("occurred_at");
    let p1 = dialect.placeholder(1);
    let p2 = dialect.placeholder(2);
    let mut sql = String::new();
    let _ = write!(
        sql,
        "SELECT {id}, {et}, {ek}, {op}, {src}, {ch}, {oa} \
         FROM {t} \
         WHERE {et} = {p1} AND {ek} = {p2} \
         ORDER BY {oa} DESC, {id} DESC",
    );
    sql
}

/// Render the `DELETE … WHERE occurred_at < $1` used by
/// [`cleanup_older_than_pool`] through the dialect emitter.
///
/// SQLite gets a second leg that normalises the **stored** value before
/// comparing, so the sweep is correct whether a row holds the old
/// `CURRENT_TIMESTAMP` shape or the RFC3339 one written today. The
/// `migrate` sweep converts old rows, but this is a DELETE and runs
/// before migrations on an upgraded install, where a legacy value sorts
/// below every canonical cutoff and history would be destroyed.
///
/// The shape matters, and two simpler ones are wrong:
///
/// - The leading range must stay bare so the `occurred_at` index is used.
///   Wrapping the column in `strftime` forces a full scan, O(table)
///   instead of O(rows deleted).
/// - Comparing the cutoff in both spellings over-deletes: `' '` sorts
///   below `'T'`, so every same-date legacy row looks older than a
///   canonical cutoff, whatever its clock time.
///
/// So the second leg filters the first rather than replacing it, and it
/// normalises instead of matching known spellings. A width-keyed `LIKE`
/// missed `2026-09-20 08:00:00.123456` — the dump spelling, and what
/// sqlx writes for a bound `NaiveDateTime` — and deleted rows hours
/// after the cutoff.
fn audit_cleanup_older_than_sql(dialect: &dyn crate::sql::Dialect) -> String {
    let t = dialect.quote_ident("rustango_audit_log");
    let oa = dialect.quote_ident("occurred_at");
    let p1 = dialect.placeholder(1);
    if dialect.name() == "sqlite" {
        let fmt = crate::sql::SQLITE_DATETIME_FORMAT;
        let p2 = dialect.placeholder(2);
        return format!(
            "DELETE FROM {t} WHERE {oa} < {p1} \
             AND strftime('{fmt}', {oa}) < {p2}"
        );
    }
    format!("DELETE FROM {t} WHERE {oa} < {p1}")
}

/// Test hook for [`audit_cleanup_older_than_sql`]. The index-plan guard
/// must check the renderer's own SQL: a guard that retypes the statement
/// cannot notice the statement changing.
#[doc(hidden)]
#[must_use]
pub fn __test_cleanup_older_than_sql(dialect: &dyn crate::sql::Dialect) -> String {
    audit_cleanup_older_than_sql(dialect)
}

/// Render the per-row retention DELETE used by
/// [`cleanup_keep_last_n_pool`]. `ROW_NUMBER() OVER (PARTITION BY)` works
/// on PG, MySQL 8+ and SQLite 3.25+, so only quoting and placeholders
/// differ.
fn audit_cleanup_keep_last_n_sql(dialect: &dyn crate::sql::Dialect) -> String {
    let t = dialect.quote_ident("rustango_audit_log");
    let id = dialect.quote_ident("id");
    let et = dialect.quote_ident("entity_table");
    let ek = dialect.quote_ident("entity_pk");
    let oa = dialect.quote_ident("occurred_at");
    let p1 = dialect.placeholder(1);
    format!(
        "DELETE FROM {t} WHERE {id} IN ( \
            SELECT {id} FROM ( \
              SELECT {id}, \
                     ROW_NUMBER() OVER ( \
                         PARTITION BY {et}, {ek} \
                         ORDER BY {oa} DESC, {id} DESC \
                     ) AS _rn \
              FROM {t} \
            ) ranked \
            WHERE _rn > {p1} \
         )"
    )
}

/// Read every audit entry for one `(entity_table, entity_pk)` pair,
/// newest first. Used by the admin's per-row audit trail panel.
///
/// Postgres-typed, kept for older call sites; it delegates to
/// [`fetch_for_entity_pool`], which is the one to use on any backend.
///
/// # Errors
/// Driver / SQL failures.
#[cfg(feature = "postgres")]
pub async fn fetch_for_entity(
    pool: &PgPool,
    entity_table: &str,
    entity_pk: &str,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    fetch_for_entity_pool(
        &crate::sql::Pool::from(pool.clone()),
        entity_table,
        entity_pk,
    )
    .await
}

/// Schema-registration model for `rustango_audit_log`, so
/// `makemigrations` owns the audit-log schema. Reads go through
/// [`AuditEntry`], which has the per-dialect `changes`-column decoders.
/// The two indexes here are the `(entity_table, entity_pk)` composite and
/// one on `occurred_at`.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_audit_log",
    index_together = "entity_table, entity_pk"
)]
#[allow(dead_code)]
pub struct AuditLog {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,
    // `entity_table` + `entity_pk` are the `index_together` key, so they
    // MUST have a max_length: MySQL cannot index an unbounded TEXT column
    // without a prefix length (error 1170).
    #[rustango(max_length = 255)]
    pub entity_table: String,
    #[rustango(max_length = 255)]
    pub entity_pk: String,
    #[rustango(max_length = 32)]
    pub operation: String,
    #[rustango(max_length = 255)]
    pub source: String,
    pub changes: serde_json::Value,
    #[rustango(index, default = "now()")]
    pub occurred_at: chrono::DateTime<chrono::Utc>,
}

/// Decoded audit-log row.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: i64,
    pub entity_table: String,
    pub entity_pk: String,
    pub operation: String,
    pub source: String,
    pub changes: Value,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(feature = "postgres")]
impl AuditEntry {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            entity_table: row.try_get("entity_table")?,
            entity_pk: row.try_get("entity_pk")?,
            operation: row.try_get("operation")?,
            source: row.try_get("source")?,
            changes: row.try_get("changes")?,
            occurred_at: row.try_get("occurred_at")?,
        })
    }
}

/// Per-backend row decoder. The `changes` column is JSONB on PG (decodes
/// straight to `Value`), JSON on MySQL (via `sqlx::types::Json<Value>`)
/// and TEXT on SQLite (parsed with `serde_json::from_str`).
#[cfg(feature = "mysql")]
impl AuditEntry {
    fn from_my_row(row: &sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row as _;
        let changes: sqlx::types::Json<Value> = row.try_get("changes")?;
        Ok(Self {
            id: row.try_get("id")?,
            entity_table: row.try_get("entity_table")?,
            entity_pk: row.try_get("entity_pk")?,
            operation: row.try_get("operation")?,
            source: row.try_get("source")?,
            changes: changes.0,
            occurred_at: row.try_get("occurred_at")?,
        })
    }
}

#[cfg(feature = "sqlite")]
impl AuditEntry {
    fn from_sq_row(row: &sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row as _;
        let changes_text: String = row.try_get("changes")?;
        let changes: Value = serde_json::from_str(&changes_text).map_err(|e| {
            sqlx::Error::Decode(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("audit `changes` is not valid JSON: {e}"),
            )))
        })?;
        Ok(Self {
            id: row.try_get("id")?,
            entity_table: row.try_get("entity_table")?,
            entity_pk: row.try_get("entity_pk")?,
            operation: row.try_get("operation")?,
            source: row.try_get("source")?,
            changes,
            occurred_at: row.try_get("occurred_at")?,
        })
    }
}

/// Delete audit entries older than `cutoff_days` from `pool`'s audit
/// table. Returns the number of rows removed.
///
/// Nothing schedules this for you — wire it into a cron job or a
/// maintenance task. Each tenant's audit table is its own retention
/// boundary, so this only touches the tenant `pool` points at.
///
/// `cutoff_days = 0` clears the whole table. A negative value clamps to 0.
///
/// Postgres-typed; [`cleanup_older_than_pool`] works on any backend.
///
/// The cutoff comes from Rust, not `NOW()`. Since [`emit_one`] binds
/// `occurred_at`, a database-side `NOW()` would compare two clocks: if
/// the app host runs ahead, its rows sit in the database's future and a
/// zero-day sweep deletes none of them.
///
/// # Errors
/// Driver / SQL failures from the DELETE.
#[cfg(feature = "postgres")]
pub async fn cleanup_older_than(pool: &PgPool, cutoff_days: i64) -> Result<u64, sqlx::Error> {
    let cutoff = cutoff_days.max(0);
    let cutoff_ts = chrono::Utc::now() - chrono::Duration::days(cutoff);
    let result = sqlx::query(r#"DELETE FROM "rustango_audit_log" WHERE "occurred_at" < $1"#)
        .bind(cutoff_ts)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Per-row retention: keep the `keep` newest audit entries for each
/// `(entity_table, entity_pk)` pair and delete the rest. Use it when you
/// must keep the edit chain of every row but cap how far the table grows.
///
/// One window-function DELETE does the work: each entry gets a
/// `ROW_NUMBER()` ordered by `occurred_at DESC, id DESC`, and rows ranked
/// above `keep` are dropped. One round-trip, however many pairs the table
/// holds.
///
/// `keep = 0` clears the whole table; negative values clamp to 0. Returns
/// the number of rows removed.
///
/// Postgres-typed; [`cleanup_keep_last_n_pool`] works on any backend.
///
/// # Errors
/// Driver / SQL failures from the DELETE.
#[cfg(feature = "postgres")]
pub async fn cleanup_keep_last_n(pool: &PgPool, keep: i64) -> Result<u64, sqlx::Error> {
    let keep = keep.max(0);
    let result = sqlx::query(
        r#"DELETE FROM "rustango_audit_log" WHERE "id" IN (
              SELECT "id" FROM (
                SELECT "id",
                       ROW_NUMBER() OVER (
                           PARTITION BY "entity_table", "entity_pk"
                           ORDER BY "occurred_at" DESC, "id" DESC
                       ) AS _rn
                FROM "rustango_audit_log"
              ) ranked
              WHERE _rn > $1
           )"#,
    )
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Make sure the table exists in `pool`'s database or schema. Does nothing
/// when it is already there. Handy in tests and ad-hoc setup.
///
/// Postgres-typed; [`ensure_table_pool`] works on any backend.
///
/// # Errors
/// Driver / SQL failures from the emitted DDL.
#[cfg(feature = "postgres")]
pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    ensure_table_pool(&crate::sql::Pool::Postgres(pool.clone())).await
}

// ============================================================ all-backend audit

/// Create the audit-log table on any backend, sending the per-dialect DDL
/// through the right driver.
///
/// MySQL has no `CREATE INDEX IF NOT EXISTS`, so duplicate-index errors
/// are ignored and the call stays idempotent.
///
/// # Errors
/// Driver / SQL failures, other than the ignored duplicate-index ones.
pub async fn ensure_table_pool(pool: &crate::sql::Pool) -> Result<(), sqlx::Error> {
    // Render the table and its indexes from `AuditLog::SCHEMA` through the
    // migration path, so this cannot drift from the model. The table is
    // shared per database, so this stays as a defensive helper next to the
    // system migrations.
    use crate::core::Model as _;
    let snapshot = crate::migrate::SchemaSnapshot::from_models(&[AuditLog::SCHEMA]);
    crate::migrate::apply_idempotent(pool, &snapshot).await?;
    Ok(())
}

/// MySQL counterpart of [`emit_one`] — `?` placeholders and backtick
/// quoting. Used for audited writes over a MySQL transaction.
///
/// # Errors
/// Driver / SQL failures from the INSERT.
#[cfg(feature = "mysql")]
pub async fn emit_one_my<'c, E>(executor: E, entry: &PendingEntry) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
{
    emit_one_my_via(executor, entry, Via::Unknown).await
}

#[cfg(feature = "mysql")]
async fn emit_one_my_via<'c, E>(
    executor: E,
    entry: &PendingEntry,
    via: Via<'_>,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
{
    // `occurred_at` is bound, not defaulted — see `emit_one`.
    sqlx::query(
        r#"INSERT INTO `rustango_audit_log`
              (`entity_table`, `entity_pk`, `operation`, `source`, `changes`, `occurred_at`)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(source_token(&entry.source, via))
    .bind(sqlx::types::Json(&entry.changes))
    .bind(chrono::Utc::now())
    .execute(executor)
    .await?;
    Ok(())
}

/// SQLite counterpart of [`emit_one`] — double-quoted identifiers and `?`
/// placeholders. The `changes` JSON goes into a TEXT column.
///
/// # Errors
/// Driver / SQL failures from the INSERT.
#[cfg(feature = "sqlite")]
pub async fn emit_one_sqlite<'c, E>(executor: E, entry: &PendingEntry) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    emit_one_sqlite_via(executor, entry, Via::Unknown).await
}

#[cfg(feature = "sqlite")]
async fn emit_one_sqlite_via<'c, E>(
    executor: E,
    entry: &PendingEntry,
    via: Via<'_>,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    // `occurred_at` is bound, not defaulted — see `emit_one`. This is the
    // dialect the rule exists for: an upgraded database's default cannot
    // be replaced, so a defaulted write here would store a legacy-shaped
    // value among canonical ones, forever.
    sqlx::query(
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes", "occurred_at")
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(source_token(&entry.source, via))
    .bind(sqlx::types::Json(&entry.changes))
    .bind(crate::sql::encode_datetime(chrono::Utc::now()))
    .execute(executor)
    .await?;
    Ok(())
}

/// Per-row audit emit over [`crate::sql::Pool`], dispatching to the right
/// backend helper. **It does not share a transaction with the data
/// write.** For that, open a transaction yourself and call the
/// per-backend `emit_one*` on it.
///
/// # Errors
/// As [`emit_one`].
pub async fn emit_one_pool(
    pool: &crate::sql::Pool,
    entry: &PendingEntry,
) -> Result<(), sqlx::Error> {
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => emit_one_pg(pg, entry, Via::Pool(pool)).await,
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => emit_one_my_via(my, entry, Via::Pool(pool)).await,
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => emit_one_sqlite_via(sq, entry, Via::Pool(pool)).await,
    }
}

/// Codename a non-superuser needs to read the admin audit feed. Rows
/// are still limited to tables they hold `{table}.view` on. Not a CRUD
/// action, so no table's `{table}.view` grants it (#1979).
pub const VIEW_CODENAME: &str = "rustango_audit_log.view_feed";

/// Codename a non-superuser needs to run the admin audit cleanup.
/// Cleanup spans every table, whatever `{table}.view` the user holds.
pub const DELETE_CODENAME: &str = "rustango_audit_log.clean_feed";

/// A permission on the admin audit feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditPerm {
    /// [`VIEW_CODENAME`].
    View,
    /// [`DELETE_CODENAME`].
    Delete,
}

impl AuditPerm {
    /// The codename that grants it.
    #[must_use]
    pub fn codename(self) -> &'static str {
        match self {
            Self::View => VIEW_CODENAME,
            Self::Delete => DELETE_CODENAME,
        }
    }

    /// The pre-#1979 name, a model table `audit`'s own CRUD codename.
    fn legacy_codename(self) -> &'static str {
        match self {
            Self::View => "audit.view",
            Self::Delete => "audit.delete",
        }
    }

    /// `true` when `perms` grants it. The legacy name counts only while
    /// no model uses table `audit`.
    #[must_use]
    pub fn granted_by(self, perms: &std::collections::HashSet<String>) -> bool {
        perms.contains(self.codename())
            || (perms.contains(self.legacy_codename())
                && crate::core::ModelEntry::for_table("audit").is_none())
    }
}

/// Filter for the admin's audit-log activity feed. Every field is
/// optional; `None` means the column is not constrained. [`list`] and
/// [`count`] turn this into a WHERE clause.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub entity_table: Option<String>,
    pub entity_pk: Option<String>,
    pub operation: Option<String>,
    pub source: Option<String>,
}

impl AuditFilter {
    /// Collect the active filters as `(column, value)` pairs. The order is
    /// stable because it drives placeholder numbering.
    fn active_pairs(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::with_capacity(4);
        if let Some(v) = self.entity_table.as_deref() {
            if !v.is_empty() {
                out.push(("entity_table", v));
            }
        }
        if let Some(v) = self.entity_pk.as_deref() {
            if !v.is_empty() {
                out.push(("entity_pk", v));
            }
        }
        if let Some(v) = self.operation.as_deref() {
            if !v.is_empty() {
                out.push(("operation", v));
            }
        }
        if let Some(v) = self.source.as_deref() {
            if !v.is_empty() {
                out.push(("source", v));
            }
        }
        out
    }
}

/// One page of audit entries matching `filter`, newest first. Works on
/// any backend.
///
/// # Errors
/// Driver / SQL failures from the SELECT, or a JSON decode failure on
/// SQLite when the `changes` TEXT column is not valid JSON.
pub async fn list(
    pool: &crate::sql::Pool,
    filter: &AuditFilter,
    page_size: i64,
    offset: i64,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    list_in(pool, filter, None, page_size, offset).await
}

/// [`list`] limited to rows whose `entity_table` is in `tables`
/// (`None` = every table). The admin feed's per-user scope.
pub(crate) async fn list_in(
    pool: &crate::sql::Pool,
    filter: &AuditFilter,
    tables: Option<&[String]>,
    page_size: i64,
    offset: i64,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    if tables.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let pairs = filter.active_pairs();
    let sql = audit_list_sql(pool.dialect(), &pairs, tables);
    let binds: Vec<&str> = pairs
        .iter()
        .map(|(_, v)| *v)
        .chain(tables.unwrap_or_default().iter().map(String::as_str))
        .collect();
    // bind+fetch cannot be shared: `sqlx::Executor` is bound per Database.
    // Only the decode is shared, via the `AuditEntry::from_*_row` helpers.
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut q = sqlx::query(&sql);
            for v in &binds {
                q = q.bind(*v);
            }
            let rows = q.bind(page_size).bind(offset).fetch_all(pg).await?;
            rows.iter().map(AuditEntry::from_row).collect()
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut q = sqlx::query(&sql);
            for v in &binds {
                q = q.bind(*v);
            }
            let rows = q.bind(page_size).bind(offset).fetch_all(my).await?;
            rows.iter().map(AuditEntry::from_my_row).collect()
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut q = sqlx::query(&sql);
            for v in &binds {
                q = q.bind(*v);
            }
            let rows = q.bind(page_size).bind(offset).fetch_all(sq).await?;
            rows.iter().map(AuditEntry::from_sq_row).collect()
        }
    }
}

/// Total row count for the audit-log pager, using the same
/// [`AuditFilter`] as [`list`].
///
/// # Errors
/// Driver / SQL failures from the SELECT COUNT(*).
pub async fn count(pool: &crate::sql::Pool, filter: &AuditFilter) -> Result<i64, sqlx::Error> {
    count_in(pool, filter, None).await
}

/// [`count`] limited to `tables`, as [`list_in`].
pub(crate) async fn count_in(
    pool: &crate::sql::Pool,
    filter: &AuditFilter,
    tables: Option<&[String]>,
) -> Result<i64, sqlx::Error> {
    use crate::core::SqlValue;
    if tables.is_some_and(<[String]>::is_empty) {
        return Ok(0);
    }
    let pairs = filter.active_pairs();
    let t = pool.dialect().quote_ident("rustango_audit_log");
    let sql = format!(
        "SELECT COUNT(*) FROM {t}{}",
        audit_where_sql(pool.dialect(), &pairs, tables)
    );
    let binds: Vec<SqlValue> = pairs
        .iter()
        .map(|(_, v)| SqlValue::String((*v).to_owned()))
        .chain(
            tables
                .unwrap_or_default()
                .iter()
                .map(|t| SqlValue::String(t.clone())),
        )
        .collect();
    // `raw_query_pool::<(i64,)>` decodes the single COUNT column the same
    // way on every backend, so no per-backend arm is needed.
    let rows: Vec<(i64,)> = crate::sql::raw_query_pool(&sql, binds, pool)
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })?;
    // COUNT(*) always returns exactly one row.
    Ok(rows.into_iter().next().map_or(0, |t| t.0))
}

/// `(value, count)` facets for the audit-log side panel, ordered by count
/// descending then value ascending. `column` must be `entity_table`,
/// `operation` or `source`.
///
/// # Errors
/// Driver / SQL failures from the SELECT, or `Error::ColumnNotFound` when
/// `column` is not one of the three allowed names.
pub async fn facet_counts(
    pool: &crate::sql::Pool,
    column: &str,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    facet_counts_in(pool, column, None).await
}

/// [`facet_counts`] limited to `tables`, as [`list_in`].
pub(crate) async fn facet_counts_in(
    pool: &crate::sql::Pool,
    column: &str,
    tables: Option<&[String]>,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    use crate::core::SqlValue;
    // `column` is interpolated into the SQL, so it must be allowlisted.
    if !matches!(column, "entity_table" | "operation" | "source") {
        return Err(sqlx::Error::ColumnNotFound(column.to_owned()));
    }
    if tables.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let sql = audit_facet_sql(pool.dialect(), column, tables);
    let binds: Vec<SqlValue> = tables
        .unwrap_or_default()
        .iter()
        .map(|t| SqlValue::String(t.clone()))
        .collect();
    // The `(String, i64)` tuple decodes positionally, so it matches the
    // `facet_value, facet_count` column order in `audit_facet_sql`.
    crate::sql::raw_query_pool::<(String, i64)>(&sql, binds, pool)
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })
}

/// `WHERE col = ? AND … AND entity_table IN (?, …)`. Binds go `pairs`
/// values first, then `tables`, matching the text order on every dialect.
fn audit_where_sql(
    dialect: &dyn crate::sql::Dialect,
    pairs: &[(&'static str, &str)],
    tables: Option<&[String]>,
) -> String {
    use std::fmt::Write as _;
    let mut sql = String::new();
    let mut idx = 1usize;
    for (col, _) in pairs {
        let prefix = if idx == 1 { " WHERE " } else { " AND " };
        let _ = write!(
            sql,
            "{prefix}{} = {}",
            dialect.quote_ident(col),
            dialect.placeholder(idx)
        );
        idx += 1;
    }
    if let Some(tables) = tables {
        let prefix = if idx == 1 { " WHERE " } else { " AND " };
        let phs: Vec<String> = (idx..idx + tables.len())
            .map(|i| dialect.placeholder(i))
            .collect();
        let _ = write!(
            sql,
            "{prefix}{} IN ({})",
            dialect.quote_ident("entity_table"),
            phs.join(", ")
        );
    }
    sql
}

/// Render the paginated activity-feed SELECT. `pairs` gives the active
/// filter columns in a stable order, so placeholder numbering is fixed.
fn audit_list_sql(
    dialect: &dyn crate::sql::Dialect,
    pairs: &[(&'static str, &str)],
    tables: Option<&[String]>,
) -> String {
    let t = dialect.quote_ident("rustango_audit_log");
    let id = dialect.quote_ident("id");
    let et = dialect.quote_ident("entity_table");
    let ek = dialect.quote_ident("entity_pk");
    let op = dialect.quote_ident("operation");
    let src = dialect.quote_ident("source");
    let ch = dialect.quote_ident("changes");
    let oa = dialect.quote_ident("occurred_at");
    let bind_idx = 1 + pairs.len() + tables.map_or(0, <[String]>::len);
    let p_limit = dialect.placeholder(bind_idx);
    let p_offset = dialect.placeholder(bind_idx + 1);
    format!(
        "SELECT {id}, {et}, {ek}, {op}, {src}, {ch}, {oa} FROM {t}{} \
         ORDER BY {oa} DESC, {id} DESC LIMIT {p_limit} OFFSET {p_offset}",
        audit_where_sql(dialect, pairs, tables)
    )
}

/// Render the facet group-by. `column` must already be allowlisted by
/// [`facet_counts`].
fn audit_facet_sql(
    dialect: &dyn crate::sql::Dialect,
    column: &str,
    tables: Option<&[String]>,
) -> String {
    let t = dialect.quote_ident("rustango_audit_log");
    let col = dialect.quote_ident(column);
    format!(
        "SELECT {col} AS facet_value, COUNT(*) AS facet_count \
         FROM {t}{} GROUP BY {col} ORDER BY facet_count DESC, {col}",
        audit_where_sql(dialect, &[], tables)
    )
}

/// Batched audit emit on any backend: chunked multi-row INSERTs, all in
/// one transaction.
///
/// Empty input returns at once.
///
/// # Errors
/// Driver / SQL failures from the INSERT(s) or the transaction.
pub async fn emit_many_pool(
    pool: &crate::sql::Pool,
    entries: &[PendingEntry],
) -> Result<(), sqlx::Error> {
    if entries.is_empty() {
        return Ok(());
    }
    let to_sqlx = |e: crate::sql::ExecError| match e {
        crate::sql::ExecError::Driver(err) => err,
        other => sqlx::Error::Protocol(format!("{other}")),
    };
    let mut tx = crate::sql::transaction_pool(pool).await.map_err(to_sqlx)?;
    emit_many_tx(&mut tx, Via::Pool(pool), entries)
        .await
        .map_err(to_sqlx)?;
    tx.commit().await
}

/// All-backend counterpart of [`fetch_for_entity`]. The `changes` column
/// is JSON on PG and MySQL and TEXT on SQLite; both decode into
/// `serde_json::Value`, so the audit panel renders the same everywhere.
///
/// # Errors
/// Driver / SQL failures from the SELECT, or a JSON decode failure, for
/// example SQLite TEXT that is not valid JSON.
pub async fn fetch_for_entity_pool(
    pool: &crate::sql::Pool,
    entity_table: &str,
    entity_pk: &str,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    // One SELECT template for every backend; only quoting and
    // placeholders differ. Decode uses the per-backend `from_*_row`.
    let sql = audit_select_sql(pool.dialect());
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let rows = sqlx::query(&sql)
                .bind(entity_table)
                .bind(entity_pk)
                .fetch_all(pg)
                .await?;
            rows.iter().map(AuditEntry::from_row).collect()
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let rows = sqlx::query(&sql)
                .bind(entity_table)
                .bind(entity_pk)
                .fetch_all(my)
                .await?;
            rows.iter().map(AuditEntry::from_my_row).collect()
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let rows = sqlx::query(&sql)
                .bind(entity_table)
                .bind(entity_pk)
                .fetch_all(sq)
                .await?;
            rows.iter().map(AuditEntry::from_sq_row).collect()
        }
    }
}

/// All-backend counterpart of [`cleanup_older_than`]. The cutoff is
/// computed in Rust and bound, so the SQL needs no
/// `NOW() - INTERVAL '… day'`.
///
/// `cutoff_days = 0` clears the whole table. A negative value clamps to 0.
///
/// **Under tenancy, pass a tenant-scoped pool.** The audit table is
/// per-tenant, so this trims only the tenant `pool` points at. Pass a
/// registry pool in schema mode and it trims `public` alone, reports
/// success, and every tenant's history keeps growing. Fan out with
/// [`crate::tenancy::for_each_tenant`].
///
/// # Errors
/// Driver / SQL failures from the DELETE.
pub async fn cleanup_older_than_pool(
    pool: &crate::sql::Pool,
    cutoff_days: i64,
) -> Result<u64, sqlx::Error> {
    use crate::core::SqlValue;
    let cutoff = cutoff_days.max(0);
    let cutoff_ts = chrono::Utc::now() - chrono::Duration::days(cutoff);
    let sql = audit_cleanup_older_than_sql(pool.dialect());
    // SQLite binds the cutoff twice: once for the indexed range, once for
    // the second leg. Both use the canonical spelling — the second leg
    // normalises the stored column, not the cutoff, so no legacy-shaped
    // value is ever bound here.
    let mut binds = vec![SqlValue::DateTime(cutoff_ts)];
    if pool.dialect().name() == "sqlite" {
        binds.push(SqlValue::DateTime(cutoff_ts));
    }
    crate::sql::raw_execute_pool(pool, &sql, binds)
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })
}

/// All-backend counterpart of [`cleanup_keep_last_n`].
/// `ROW_NUMBER() OVER (PARTITION BY …)` works on PG, MySQL 8+ and
/// SQLite 3.25+, so only identifier quoting differs.
///
/// `keep = 0` clears the whole table; negative values clamp to 0.
///
/// # Errors
/// Driver / SQL failures from the DELETE. On MySQL 5.7 or SQLite 3.24 and
/// older you get an "unsupported window function" error; there, write
/// your own retention DELETE instead.
pub async fn cleanup_keep_last_n_pool(
    pool: &crate::sql::Pool,
    keep: i64,
) -> Result<u64, sqlx::Error> {
    use crate::core::SqlValue;
    let keep = keep.max(0);
    let sql = audit_cleanup_keep_last_n_sql(pool.dialect());
    crate::sql::raw_execute_pool(pool, &sql, vec![SqlValue::I64(keep)])
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })
}

/// Run a `DeleteQuery` and emit its audit entry in one transaction, so
/// the row and its audit record commit together. No row is written when
/// nothing was deleted. Used by the generated `Model::delete_pool`.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from compile, bind or execute, plus
/// `sqlx::Error` from the audit emit (wrapped as `ExecError::Driver`).
pub async fn delete_one_with_audit(
    pool: &crate::sql::Pool,
    query: &crate::core::DeleteQuery,
    entry: &PendingEntry,
) -> Result<u64, crate::sql::ExecError> {
    let stmt = pool.dialect().compile_delete(query)?;
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let affected = crate::sql::raw_execute_tx(&mut tx, &stmt.sql, stmt.params).await?;
    if affected > 0 {
        emit_one_tx(&mut tx, Via::Pool(pool), entry).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Emit an audit entry inside an open `PoolTx`, so callers stay on the
/// `PoolTx` API instead of unwrapping the backend variant themselves.
async fn emit_one_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    via: Via<'_>,
    entry: &PendingEntry,
) -> Result<(), sqlx::Error> {
    match tx {
        #[cfg(feature = "postgres")]
        crate::sql::PoolTx::Postgres(t) => emit_one_pg(&mut **t, entry, via).await,
        #[cfg(feature = "mysql")]
        crate::sql::PoolTx::Mysql(t) => emit_one_my_via(&mut **t, entry, via).await,
        #[cfg(feature = "sqlite")]
        crate::sql::PoolTx::Sqlite(t) => emit_one_sqlite_via(&mut **t, entry, via).await,
    }
}

/// Run an `UpdateQuery` and emit its audit entry in one transaction,
/// unless nothing was updated. Used by the generated `Model::save_pool`.
///
/// The entry is a **snapshot**: `changes` holds the values after the
/// write, with no `before` side. For a field-level diff, use
/// [`save_one_with_diff`], which runs the pre-UPDATE SELECT.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from compile, bind or execute, plus
/// `sqlx::Error` from the audit emit.
pub async fn save_one_with_audit(
    pool: &crate::sql::Pool,
    query: &crate::core::UpdateQuery,
    entry: &PendingEntry,
) -> Result<u64, crate::sql::ExecError> {
    query.validate()?;
    let stmt = pool.dialect().compile_update(query)?;
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let affected = crate::sql::raw_execute_tx(&mut tx, &stmt.sql, stmt.params).await?;
    if affected > 0 {
        emit_one_tx(&mut tx, Via::Pool(pool), entry).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Who writes an admin write's audit entry.
#[cfg(feature = "admin")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffEmit {
    /// The helper, in the write's transaction: a failed emit undoes the write.
    InTx,
    /// The caller, which gets the entry back: best-effort after commit,
    /// or batched into its own transaction.
    Deferred,
}

#[cfg(feature = "admin")]
impl DiffEmit {
    /// An `audit(...)` model audits in the write's tx; others best-effort.
    pub(crate) fn for_model(model: &crate::core::ModelSchema) -> Self {
        if model.audit_track.is_some() {
            Self::InTx
        } else {
            Self::Deferred
        }
    }
}

/// Write `entries` in the caller's `tx`: a failed emit fails the write.
#[cfg(feature = "admin")]
pub(crate) async fn emit_in_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    pool: &crate::sql::Pool,
    entries: &[PendingEntry],
) -> Result<(), crate::sql::ExecError> {
    emit_many_tx(tx, Via::Pool(pool), entries).await
}

/// Outcome of [`update_one_with_row_diff_tx`].
#[cfg(feature = "admin")]
#[derive(Debug)]
pub(crate) enum RowDiffWrite {
    /// The locked read found no row: nothing was written.
    Gone,
    /// The UPDATE ran; `deferred` is the entry left for the caller.
    Written { deferred: Option<PendingEntry> },
}

/// Lock the row `before` selects in `tx`, build the entry from that row,
/// then UPDATE. A concurrent edit cannot slip between the diff's read and
/// the write. The caller commits, so later writes can share the tx.
#[cfg(feature = "admin")]
pub(crate) async fn update_one_with_row_diff_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    pool: &crate::sql::Pool,
    query: &crate::core::UpdateQuery,
    mut before: crate::core::SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
    entry_of: impl FnOnce(&serde_json::Value) -> Option<PendingEntry>,
    emit: DiffEmit,
) -> Result<RowDiffWrite, crate::sql::ExecError> {
    query.validate()?;
    let stmt = pool.dialect().compile_update(query)?;
    before.lock_mode = Some(crate::core::LockMode {
        silent_on_sqlite: true,
        ..crate::core::LockMode::default()
    });
    let Some(row) = crate::sql::select_one_row_as_json_tx(tx, &before, fields).await? else {
        return Ok(RowDiffWrite::Gone);
    };
    let entry = entry_of(&row);
    let affected = crate::sql::raw_execute_tx(tx, &stmt.sql, stmt.params).await?;
    let entry = entry.filter(|_| affected > 0);
    if let (DiffEmit::InTx, Some(entry)) = (emit, &entry) {
        emit_one_tx(tx, Via::Pool(pool), entry).await?;
    }
    Ok(RowDiffWrite::Written {
        deferred: entry.filter(|_| emit == DiffEmit::Deferred),
    })
}

/// INSERT `query`, then build its entry from the new PK. With
/// [`DiffEmit::InTx`] the entry commits with the row (#2101); otherwise
/// it comes back for the caller's best-effort emit.
#[cfg(feature = "admin")]
pub(crate) async fn insert_one_with_entry(
    pool: &crate::sql::Pool,
    query: &crate::core::InsertQuery,
    pk_field: &crate::core::FieldSchema,
    entry_of: impl FnOnce(&crate::core::SqlValue) -> PendingEntry,
    emit: DiffEmit,
) -> Result<(crate::core::SqlValue, Option<PendingEntry>), crate::sql::ExecError> {
    let mut tx = crate::sql::write_transaction_pool(pool).await?;
    let written = insert_one_with_entry_tx(&mut tx, pool, query, pk_field, entry_of, emit).await?;
    tx.commit().await?;
    Ok(written)
}

/// [`insert_one_with_entry`] in the caller's `tx`; the caller commits.
#[cfg(feature = "admin")]
pub(crate) async fn insert_one_with_entry_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    pool: &crate::sql::Pool,
    query: &crate::core::InsertQuery,
    pk_field: &crate::core::FieldSchema,
    entry_of: impl FnOnce(&crate::core::SqlValue) -> PendingEntry,
    emit: DiffEmit,
) -> Result<(crate::core::SqlValue, Option<PendingEntry>), crate::sql::ExecError> {
    let returning = crate::sql::insert_returning_tx(tx, query).await?;
    let pk = crate::sql::inserted_pk(query, &returning, pk_field)?;
    let entry = entry_of(&pk);
    if emit == DiffEmit::InTx {
        emit_one_tx(tx, Via::Pool(pool), &entry).await?;
    }
    Ok((pk, (emit == DiffEmit::Deferred).then_some(entry)))
}

/// Run an `InsertQuery`, write the assigned PK back into `model`, then
/// emit `entry(model)` in the same transaction, so the audit row carries
/// the real PK. Used by the generated `Model::insert_pool` for audited
/// models.
///
/// MySQL fills in only the first `Auto<T>` field (one `LAST_INSERT_ID()`),
/// so other tracked `Auto` and generated fields audit as `null` there.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from compile, bind, execute or PK
/// decode, plus `sqlx::Error` from the audit emit.
pub async fn insert_one_with_audit<M>(
    pool: &crate::sql::Pool,
    query: &crate::core::InsertQuery,
    model: &mut M,
    entry: impl FnOnce(&M) -> PendingEntry,
) -> Result<(), crate::sql::ExecError>
where
    M: crate::sql::AssignAutoPkPool,
{
    // `insert_returning_tx` already handles each backend's return shape.
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let returning = crate::sql::insert_returning_tx(&mut tx, query).await?;
    crate::sql::apply_auto_pk(returning, model)?;
    emit_one_tx(&mut tx, Via::Pool(pool), &entry(model)).await?;
    tx.commit().await?;
    Ok(())
}

/// Emit a batch of entries inside an open `PoolTx`, in bind-limit-sized
/// multi-row INSERTs.
async fn emit_many_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    via: Via<'_>,
    entries: &[PendingEntry],
) -> Result<(), crate::sql::ExecError> {
    let per_insert = audit_rows_per_insert(tx.dialect());
    for chunk in entries.chunks(per_insert) {
        let stmt = audit_insert_stmt(tx.dialect(), chunk, via)?;
        crate::sql::raw_execute_tx(tx, &stmt.sql, stmt.params).await?;
    }
    Ok(())
}

/// PKs per bulk-write statement: under SQLite's oldest bind limit (999).
const BULK_AUDIT_CHUNK: usize = 500;

/// `pk IN (pks)` on `model`.
fn pk_in(
    model: &'static crate::core::ModelSchema,
    pks: Vec<crate::core::SqlValue>,
) -> Result<crate::core::WhereExpr, crate::sql::ExecError> {
    let pk = model
        .primary_key()
        .ok_or(crate::sql::ExecError::MissingPrimaryKey { table: model.table })?;
    Ok(crate::core::WhereExpr::Predicate(crate::core::Filter::new(
        pk.column,
        crate::core::Op::In,
        crate::core::SqlValue::List(pks),
    )))
}

/// Rows matching `where_clause`, read in `tx` (`FOR UPDATE` on PG/MySQL).
async fn rows_in_tx<M>(
    tx: &mut crate::sql::PoolTx<'_>,
    model: &'static crate::core::ModelSchema,
    where_clause: crate::core::WhereExpr,
    lock: bool,
) -> Result<Vec<M>, crate::sql::ExecError>
where
    M: crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    let mut select = crate::core::SelectQuery::new(model).where_clause(where_clause);
    if lock {
        select.lock_mode = Some(crate::core::LockMode {
            silent_on_sqlite: true,
            ..crate::core::LockMode::default()
        });
    }
    crate::sql::select_rows_tx_with_related::<M>(tx, &select).await
}

/// `true` when only the row's own columns decide whether it matches, so
/// writing other rows cannot move it in or out of the set.
fn is_row_local(where_clause: &crate::core::WhereExpr) -> bool {
    use crate::core::WhereExpr as W;
    match where_clause {
        W::Predicate(_) => true,
        W::And(items) | W::Or(items) | W::Xor(items) => items.iter().all(is_row_local),
        W::Not(child) => is_row_local(child),
        _ => false,
    }
}

/// Rows a bulk write goes through, one locked page of
/// [`BULK_AUDIT_CHUNK`] at a time, inside the write's transaction.
enum Pages<M> {
    /// Row-local WHERE: `… AND pk > last ORDER BY pk LIMIT n FOR UPDATE`.
    Keyset {
        where_clause: crate::core::WhereExpr,
        last: Option<crate::core::SqlValue>,
        done: bool,
    },
    /// The WHERE reads other rows (a subquery, e.g. a `limit()` bound),
    /// which our own writes would shift, so the set is read once.
    Pinned(std::vec::IntoIter<M>),
}

impl<M> Pages<M>
where
    M: crate::sql::HasPkValue
        + crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    async fn start(
        tx: &mut crate::sql::PoolTx<'_>,
        model: &'static crate::core::ModelSchema,
        where_clause: &crate::core::WhereExpr,
    ) -> Result<Self, crate::sql::ExecError> {
        if is_row_local(where_clause) {
            return Ok(Self::Keyset {
                where_clause: where_clause.clone(),
                last: None,
                done: false,
            });
        }
        let rows: Vec<M> = rows_in_tx(tx, model, where_clause.clone(), true).await?;
        Ok(Self::Pinned(rows.into_iter()))
    }

    /// The next page; empty when the set is exhausted.
    async fn next(
        &mut self,
        tx: &mut crate::sql::PoolTx<'_>,
        model: &'static crate::core::ModelSchema,
    ) -> Result<Vec<M>, crate::sql::ExecError> {
        match self {
            Self::Pinned(rows) => Ok(rows.by_ref().take(BULK_AUDIT_CHUNK).collect()),
            Self::Keyset {
                where_clause,
                last,
                done,
            } => {
                if *done {
                    return Ok(Vec::new());
                }
                let pk = model
                    .primary_key()
                    .ok_or(crate::sql::ExecError::MissingPrimaryKey { table: model.table })?;
                let mut page_where = where_clause.clone();
                if let Some(last) = last.take() {
                    page_where.push_and(crate::core::WhereExpr::Predicate(
                        crate::core::Filter::new(pk.column, crate::core::Op::Gt, last),
                    ));
                }
                let mut select = crate::core::SelectQuery::new(model).where_clause(page_where);
                select.order_by = vec![crate::core::OrderItem::column(pk.column, false)];
                select.limit = Some(BULK_AUDIT_CHUNK as i64);
                select.lock_mode = Some(crate::core::LockMode {
                    silent_on_sqlite: true,
                    ..crate::core::LockMode::default()
                });
                let rows: Vec<M> =
                    crate::sql::select_rows_tx_with_related::<M>(tx, &select).await?;
                *done = rows.len() < BULK_AUDIT_CHUNK;
                *last = rows.last().map(M::__rustango_pk_value_impl);
                Ok(rows)
            }
        }
    }
}

/// Run a bulk `DeleteQuery` with one `Delete` audit row per deleted row,
/// all in one transaction. Matching rows are locked and deleted by PK a
/// page at a time, so the audit set is exactly the deleted set.
///
/// # Errors
/// As [`delete_one_with_audit`], plus the pre-delete SELECT.
pub async fn delete_many_with_audit<M>(
    pool: &crate::sql::Pool,
    query: &crate::core::DeleteQuery,
    entry: impl Fn(&M) -> PendingEntry,
) -> Result<u64, crate::sql::ExecError>
where
    M: crate::sql::HasPkValue
        + crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    let mut tx = crate::sql::write_transaction_pool(pool).await?;
    let mut pages = Pages::<M>::start(&mut tx, query.model, &query.where_clause).await?;
    let mut affected = 0;
    loop {
        let chunk = pages.next(&mut tx, query.model).await?;
        if chunk.is_empty() {
            break;
        }
        let pks = chunk.iter().map(M::__rustango_pk_value_impl).collect();
        let delete = crate::core::DeleteQuery {
            model: query.model,
            where_clause: pk_in(query.model, pks)?,
        };
        affected += crate::sql::delete_tx(&mut tx, &delete).await?;
        let entries: Vec<PendingEntry> = chunk.iter().map(&entry).collect();
        emit_many_tx(&mut tx, Via::Pool(pool), &entries).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Run a bulk `UpdateQuery` with one `Update` audit row per updated row,
/// all in one transaction. Rows are locked a page at a time, updated by
/// PK, then re-read so each entry is an after-write snapshot, as on `save_pool`.
///
/// # Errors
/// As [`save_one_with_audit`], plus the SELECTs around the update.
pub async fn update_many_with_audit<M>(
    pool: &crate::sql::Pool,
    query: &crate::core::UpdateQuery,
    entry: impl Fn(&M) -> PendingEntry,
) -> Result<u64, crate::sql::ExecError>
where
    M: crate::sql::HasPkValue
        + crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    let pk = query
        .model
        .primary_key()
        .ok_or(crate::sql::ExecError::MissingPrimaryKey {
            table: query.model.table,
        })?;
    // The after-write re-read is by PK, so a PK change would go unaudited.
    if query.set.iter().any(|a| a.column == pk.column) {
        return Err(crate::sql::ExecError::AuditUnsupported {
            table: query.model.table,
            reason: "a bulk update cannot change the primary key",
        });
    }
    let mut tx = crate::sql::write_transaction_pool(pool).await?;
    let mut pages = Pages::<M>::start(&mut tx, query.model, &query.where_clause).await?;
    let mut affected = 0;
    loop {
        let chunk = pages.next(&mut tx, query.model).await?;
        if chunk.is_empty() {
            break;
        }
        let pks: Vec<crate::core::SqlValue> =
            chunk.iter().map(M::__rustango_pk_value_impl).collect();
        let update = crate::core::UpdateQuery::new(
            query.model,
            query.set.clone(),
            pk_in(query.model, pks.clone())?,
        );
        affected += crate::sql::update_tx(&mut tx, &update).await?;
        let after: Vec<M> =
            rows_in_tx(&mut tx, query.model, pk_in(query.model, pks)?, false).await?;
        let entries: Vec<PendingEntry> = after.iter().map(&entry).collect();
        emit_many_tx(&mut tx, Via::Pool(pool), &entries).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Audited `UPDATE` runner a model hands to generic code, through
/// `Model::__rustango_audited_update`.
pub type AuditedUpdate = for<'a> fn(
    &'a crate::sql::Pool,
    &'a crate::core::UpdateQuery,
    AuditOp,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<u64, crate::sql::ExecError>> + Send + 'a>,
>;

/// Audited `DELETE` runner, through `Model::__rustango_audited_delete`.
pub type AuditedDelete = for<'a> fn(
    &'a crate::sql::Pool,
    &'a crate::core::DeleteQuery,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<u64, crate::sql::ExecError>> + Send + 'a>,
>;

/// Audited `create` recorder, through `Model::__rustango_audited_create`:
/// re-reads the new row by PK in the insert's transaction and emits its entry.
pub type AuditedCreate = for<'a, 't> fn(
    &'a mut crate::sql::PoolTx<'t>,
    crate::core::SqlValue,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), crate::sql::ExecError>> + Send + 'a>,
>;

/// Audited `update` recorder, through `Model::__rustango_audited_update_record`:
/// re-reads the updated row by PK in the update's transaction (#2010).
pub type AuditedUpdateRecord = for<'a, 't> fn(
    &'a mut crate::sql::PoolTx<'t>,
    crate::core::SqlValue,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), crate::sql::ExecError>> + Send + 'a>,
>;

/// A one-row update that can run inside a caller's transaction with its
/// audit entry (#2010). Absent for a model whose audit runs only on a pool.
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
#[derive(Clone, Copy)]
pub(crate) struct TxUpdate(Option<AuditedUpdateRecord>);

#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
impl TxUpdate {
    /// `None` when `model` audits updates but has no in-transaction
    /// recorder (a hand-written `ModelEntry`): its update must go through
    /// [`update`], or its audit entry would be lost.
    pub(crate) fn for_model(model: &crate::core::ModelSchema) -> Option<Self> {
        Self::for_entry(crate::core::ModelEntry::for_schema(model))
    }

    fn for_entry(entry: Option<&crate::core::ModelEntry>) -> Option<Self> {
        let Some(entry) = entry else {
            return Some(Self(None));
        };
        match (entry.audited_update_record(), entry.audited_update()) {
            (Some(record), _) => Some(Self(Some(record))),
            (None, Some(_)) => None,
            (None, None) => Some(Self(None)),
        }
    }

    /// Run `query` in `tx`, then record the row `pk` re-read.
    ///
    /// # Errors
    /// As [`crate::sql::update_tx`], plus the audit write.
    pub(crate) async fn run(
        self,
        tx: &mut crate::sql::PoolTx<'_>,
        pool: &crate::sql::Pool,
        query: &crate::core::UpdateQuery,
        pk: crate::core::SqlValue,
    ) -> Result<u64, crate::sql::ExecError> {
        let n = crate::sql::update_tx(tx, query).await?;
        if let (true, Some(record)) = (n > 0, self.0) {
            writing_via(pool, record(tx, pk)).await.map_err(|e| {
                crate::sql::ExecError::AuditWrite {
                    table: query.model.table,
                    source: Box::new(e),
                }
            })?;
        }
        Ok(n)
    }
}

/// Emit a `Create` entry for the row with primary key `pk`, read in `tx`.
///
/// # Errors
/// As the re-read SELECT and the emit.
pub async fn record_create<M>(
    tx: &mut crate::sql::PoolTx<'_>,
    model: &'static crate::core::ModelSchema,
    pk: crate::core::SqlValue,
    entry: impl Fn(&M) -> PendingEntry,
) -> Result<(), crate::sql::ExecError>
where
    M: crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    record_by_pk(
        tx,
        model,
        pk,
        entry,
        "the created row was not found by its primary key",
    )
    .await
}

/// Emit an entry for the row with primary key `pk` after an update in
/// `tx`, read in `tx` (#2010).
///
/// # Errors
/// As the re-read SELECT and the emit.
#[doc(hidden)]
pub async fn record_update<M>(
    tx: &mut crate::sql::PoolTx<'_>,
    model: &'static crate::core::ModelSchema,
    pk: crate::core::SqlValue,
    entry: impl Fn(&M) -> PendingEntry,
) -> Result<(), crate::sql::ExecError>
where
    M: crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    record_by_pk(
        tx,
        model,
        pk,
        entry,
        "the updated row was not found by its primary key",
    )
    .await
}

async fn record_by_pk<M>(
    tx: &mut crate::sql::PoolTx<'_>,
    model: &'static crate::core::ModelSchema,
    pk: crate::core::SqlValue,
    entry: impl Fn(&M) -> PendingEntry,
    missing: &'static str,
) -> Result<(), crate::sql::ExecError>
where
    M: crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    let rows: Vec<M> = rows_in_tx(tx, model, pk_in(model, vec![pk])?, false).await?;
    if rows.len() != 1 {
        return Err(crate::sql::ExecError::AuditUnsupported {
            table: model.table,
            reason: missing,
        });
    }
    let entries: Vec<PendingEntry> = rows.iter().map(&entry).collect();
    emit_many_tx(tx, Via::Scoped, &entries).await
}

#[cfg_attr(
    not(any(feature = "admin", feature = "tenancy", feature = "forms")),
    allow(dead_code)
)]
fn audited_create(query: &crate::core::InsertQuery) -> Option<AuditedCreate> {
    crate::core::ModelEntry::for_schema(query.model).and_then(|e| e.audited_create())
}

/// `true` when inserts on `model` write a `create` audit row.
#[cfg_attr(not(feature = "template_views"), allow(dead_code))]
pub(crate) fn audits_creates(model: &crate::core::ModelSchema) -> bool {
    crate::core::ModelEntry::for_schema(model).is_some_and(|e| e.audited_create().is_some())
}

/// Run `query` and return the new row's PK, writing a `create` audit row
/// in the same transaction when its model is audited (#1816).
///
/// Crate-private: an `on_conflict` insert may write no row, which the
/// create audit can't tell apart from a new one.
///
/// # Errors
/// As [`crate::sql::insert_returning_pool`], plus the audit write.
#[cfg_attr(
    not(any(feature = "admin", feature = "tenancy", feature = "forms")),
    allow(dead_code)
)]
pub(crate) async fn insert(
    pool: &crate::sql::Pool,
    query: &crate::core::InsertQuery,
    pk_field: &crate::core::FieldSchema,
) -> Result<crate::core::SqlValue, crate::sql::ExecError> {
    Ok(insert_returning(pool, query, pk_field).await?.0)
}

/// [`insert`], also handing back the RETURNING row.
///
/// # Errors
/// As [`insert`].
#[cfg_attr(
    not(any(feature = "admin", feature = "tenancy", feature = "forms")),
    allow(dead_code)
)]
pub(crate) async fn insert_returning(
    pool: &crate::sql::Pool,
    query: &crate::core::InsertQuery,
    pk_field: &crate::core::FieldSchema,
) -> Result<(crate::core::SqlValue, crate::sql::InsertReturningPool), crate::sql::ExecError> {
    if audited_create(query).is_none() {
        let returning = crate::sql::insert_returning_pool(pool, query).await?;
        let pk = crate::sql::inserted_pk(query, &returning, pk_field)?;
        return Ok((pk, returning));
    }
    let mut tx = crate::sql::transaction_pool(pool).await?;
    // The create recorder only has the transaction.
    let inserted = writing_via(pool, insert_returning_tx(&mut tx, query, pk_field)).await?;
    tx.commit().await?;
    Ok(inserted)
}

/// [`insert`] inside an open transaction.
///
/// # Errors
/// As [`crate::sql::insert_returning_tx`], plus the audit write.
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) async fn insert_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    query: &crate::core::InsertQuery,
    pk_field: &crate::core::FieldSchema,
) -> Result<crate::core::SqlValue, crate::sql::ExecError> {
    Ok(insert_returning_tx(tx, query, pk_field).await?.0)
}

#[cfg_attr(
    not(any(feature = "admin", feature = "tenancy", feature = "forms")),
    allow(dead_code)
)]
async fn insert_returning_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    query: &crate::core::InsertQuery,
    pk_field: &crate::core::FieldSchema,
) -> Result<(crate::core::SqlValue, crate::sql::InsertReturningPool), crate::sql::ExecError> {
    let returning = crate::sql::insert_returning_tx(tx, query).await?;
    let pk = crate::sql::inserted_pk(query, &returning, pk_field)?;
    if let Some(record) = audited_create(query) {
        record(tx, pk.clone())
            .await
            .map_err(|e| crate::sql::ExecError::AuditWrite {
                table: query.model.table,
                source: Box::new(e),
            })?;
    }
    Ok((pk, returning))
}

/// Run `query`, auditing each row when its model is audited (#1794). The
/// choke point for schema-driven writes that have no `M` to call.
///
/// # Errors
/// As [`update_many_with_audit`] or [`crate::sql::update_pool`].
pub async fn update(
    pool: &crate::sql::Pool,
    query: &crate::core::UpdateQuery,
) -> Result<u64, crate::sql::ExecError> {
    update_as(pool, query, AuditOp::Update).await
}

/// [`update`], recording each row as `op` (soft delete and restore).
///
/// # Errors
/// As [`update`].
pub async fn update_as(
    pool: &crate::sql::Pool,
    query: &crate::core::UpdateQuery,
    op: AuditOp,
) -> Result<u64, crate::sql::ExecError> {
    match crate::core::ModelEntry::for_schema(query.model).and_then(|e| e.audited_update()) {
        Some(run) => run(pool, query, op).await,
        None => crate::sql::update_pool(pool, query).await,
    }
}

/// Run `query`, auditing each deleted row when its model is audited (#1794).
///
/// # Errors
/// As [`delete_many_with_audit`] or [`crate::sql::delete_pool`].
pub async fn delete(
    pool: &crate::sql::Pool,
    query: &crate::core::DeleteQuery,
) -> Result<u64, crate::sql::ExecError> {
    match crate::core::ModelEntry::for_schema(query.model).and_then(|e| e.audited_delete()) {
        Some(run) => run(pool, query).await,
        None => crate::sql::delete_pool(pool, query).await,
    }
}

/// Run a `BulkUpdateQuery` (`Model::bulk_update`) and one `Update` entry
/// per updated row, re-read after the write, in one transaction.
///
/// # Errors
/// As [`crate::sql::bulk_update_pool`], plus the re-read and the emit.
pub async fn bulk_update_with_audit<M>(
    pool: &crate::sql::Pool,
    query: &crate::core::BulkUpdateQuery,
    entry: impl Fn(&M) -> PendingEntry,
) -> Result<u64, crate::sql::ExecError>
where
    M: crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    if query.rows.is_empty() {
        return Ok(0);
    }
    let mut tx = crate::sql::write_transaction_pool(pool).await?;
    // Each row binds its PK and one value per update column. Small pages
    // also keep SQLite's per-row CTE lookup cheap.
    let per_row = query.update_columns.len() + 1;
    let max_rows = (tx.dialect().max_bind_params() / per_row).clamp(1, BULK_AUDIT_CHUNK);
    let mut affected = 0;
    for chunk in query.rows.chunks(max_rows) {
        let batch = crate::core::BulkUpdateQuery::new(
            query.model,
            query.update_columns.clone(),
            chunk.to_vec(),
        );
        let stmt = tx.dialect().compile_bulk_update(&batch)?;
        affected += crate::sql::raw_execute_tx(&mut tx, &stmt.sql, stmt.params).await?;
    }
    // Each row is `[pk, …update cols]`.
    let pks: Vec<crate::core::SqlValue> = query
        .rows
        .iter()
        .filter_map(|r| r.first().cloned())
        .collect();
    for chunk in pks.chunks(BULK_AUDIT_CHUNK) {
        let after: Vec<M> = rows_in_tx(
            &mut tx,
            query.model,
            pk_in(query.model, chunk.to_vec())?,
            false,
        )
        .await?;
        let entries: Vec<PendingEntry> = after.iter().map(&entry).collect();
        emit_many_tx(&mut tx, Via::Pool(pool), &entries).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Run a conflict-handling bulk insert (`bulk_upsert_pool`,
/// `bulk_insert_or_ignore_pool`) with one audit row per row it wrote, in
/// one transaction (#1795). Each row is first tried with `DoNothing`; only
/// the rows that skipped go to the `DoUpdate`, audited as `Update`.
///
/// The first pass runs one statement per row. On PG a row deleted by
/// another transaction between the passes is recorded as `Update`. MySQL
/// has no `RETURNING`, so it is refused.
///
/// # Errors
/// [`crate::sql::ExecError::AuditUnsupported`] on MySQL, else as
/// [`crate::sql::bulk_insert_pool`], plus the re-read and the emit.
pub async fn bulk_insert_with_audit<M>(
    pool: &crate::sql::Pool,
    query: &crate::core::BulkInsertQuery,
    entry: impl Fn(&M, AuditOp) -> PendingEntry,
) -> Result<(), crate::sql::ExecError>
where
    M: crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    if !pool.dialect().supports_returning() {
        return Err(crate::sql::ExecError::AuditUnsupported {
            table: query.model.table,
            reason:
                "without RETURNING a conflict-handling bulk insert cannot tell which rows it wrote",
        });
    }
    if query.rows.is_empty() {
        return Ok(());
    }
    let pk_field = query
        .model
        .primary_key()
        .ok_or(crate::sql::ExecError::MissingPrimaryKey {
            table: query.model.table,
        })?;
    let mut tx = crate::sql::write_transaction_pool(pool).await?;
    let mut created = Vec::new();
    let mut skipped = Vec::new();
    for row in &query.rows {
        let probe = crate::core::InsertQuery::new(query.model, query.columns.clone(), row.clone())
            .returning(vec![pk_field.column])
            .on_conflict(crate::core::ConflictClause::DoNothing);
        match crate::sql::insert_returning_tx(&mut tx, &probe).await {
            Ok(returning) => {
                created.push(crate::sql::inserted_pk(&probe, &returning, pk_field)?);
            }
            Err(crate::sql::ExecError::Driver(sqlx::Error::RowNotFound)) => {
                skipped.push(row.clone());
            }
            Err(e) => return Err(e),
        }
    }
    let mut updated = Vec::new();
    if let (Some(clause @ crate::core::ConflictClause::DoUpdate { .. }), false) =
        (&query.on_conflict, skipped.is_empty())
    {
        let upsert = crate::core::BulkInsertQuery::new(query.model, query.columns.clone(), skipped)
            .on_conflict(clause.clone());
        updated = crate::sql::bulk_insert_pks_tx(&mut tx, &upsert).await?;
    }
    for (pks, op) in [(created, AuditOp::Create), (updated, AuditOp::Update)] {
        for chunk in pks.chunks(BULK_AUDIT_CHUNK) {
            let rows: Vec<M> = rows_in_tx(
                &mut tx,
                query.model,
                pk_in(query.model, chunk.to_vec())?,
                false,
            )
            .await?;
            let entries: Vec<PendingEntry> = rows.iter().map(|r| entry(r, op)).collect();
            emit_many_tx(&mut tx, Via::Pool(pool), &entries).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// A single-row `INSERT … ON CONFLICT` on PG, with whether it inserted
/// (`Create`) or updated (`Update`) the row: a `DoUpdate` runs a
/// `DoNothing` pass first and upserts only when that skipped (#1795).
/// A row another transaction deletes between the passes is recorded as `Update`.
///
/// # Errors
/// As [`crate::sql::insert_returning_pool`].
#[cfg(feature = "postgres")]
pub async fn upsert_returning_on(
    conn: &mut sqlx::PgConnection,
    query: &crate::core::InsertQuery,
) -> Result<(sqlx::postgres::PgRow, AuditOp), crate::sql::ExecError> {
    use crate::sql::insert_returning_on;
    if !matches!(
        query.on_conflict,
        Some(crate::core::ConflictClause::DoUpdate { .. })
    ) {
        return Ok((
            insert_returning_on(&mut *conn, query).await?,
            AuditOp::Create,
        ));
    }
    let mut probe = query.clone();
    probe.on_conflict = Some(crate::core::ConflictClause::DoNothing);
    match insert_returning_on(&mut *conn, &probe).await {
        Ok(row) => Ok((row, AuditOp::Create)),
        Err(crate::sql::ExecError::Driver(sqlx::Error::RowNotFound)) => Ok((
            insert_returning_on(&mut *conn, query).await?,
            AuditOp::Update,
        )),
        Err(e) => Err(e),
    }
}

/// Run a table-wide statement (`Model::truncate`) and one bulk `Delete`
/// entry naming it, in one transaction. No per-row PK is known here.
///
/// # Errors
/// As [`crate::sql::raw_execute_tx`], plus the audit emit.
pub async fn truncate_with_audit(
    pool: &crate::sql::Pool,
    entity_table: &'static str,
    sql: &str,
) -> Result<u64, crate::sql::ExecError> {
    let mut tx = crate::sql::write_transaction_pool(pool).await?;
    let affected = crate::sql::raw_execute_tx(&mut tx, sql, Vec::new()).await?;
    let entry = PendingEntry {
        entity_table,
        entity_pk: String::new(),
        operation: AuditOp::Delete,
        source: current_source(),
        changes: serde_json::json!({ "bulk": "truncate" }),
    };
    emit_one_tx(&mut tx, Via::Pool(pool), &entry).await?;
    tx.commit().await?;
    Ok(affected)
}

/// Postgres bind helper, exposed so generated bodies on the audited
/// `save_pool` diff path can bind `SqlValue` arguments to a transaction.
/// Not part of the public API.
#[doc(hidden)]
#[cfg(feature = "postgres")]
pub fn __bind_value_pg(
    q: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments> {
    crate::sql::bind_query(q, value)
}

/// MySQL counterpart of [`__bind_value_pg`].
#[doc(hidden)]
#[cfg(feature = "mysql")]
pub fn __bind_value_my(
    q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    crate::sql::bind_query_my(q, value)
}

/// SQLite counterpart of [`__bind_value_pg`].
#[doc(hidden)]
#[cfg(feature = "sqlite")]
pub fn __bind_value_sqlite<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    crate::sql::bind_query_sqlite(q, value)
}

/// The BEFORE read of an audited update: `columns` of the row at `pk_value`.
#[doc(hidden)]
#[must_use]
pub fn before_image_query(
    schema: &'static crate::core::ModelSchema,
    pk_column: &'static str,
    pk_value: crate::core::SqlValue,
    columns: &[&'static str],
) -> crate::core::SelectQuery {
    let mut q = crate::core::SelectQuery::by_pk(schema, pk_column, pk_value);
    q.projection = Some(columns.to_vec());
    q
}

/// Per-row audited save with a field-level diff, on any backend. All of
/// it runs in one transaction:
///
/// 1. SELECT the tracked columns and decode them as the BEFORE pairs.
/// 2. Run the compiled UPDATE.
/// 3. Diff `after_pairs` against BEFORE.
/// 4. Emit an `Update` audit entry, then commit.
///
/// The closure argument types ([`crate::sql::PgReturningRow`] and its
/// siblings) resolve to uninhabited types when a backend feature is off,
/// so generated closure bodies still typecheck in any feature set.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from the SELECT or UPDATE, plus
/// `sqlx::Error` from the audit emit.
#[allow(clippy::too_many_arguments)]
pub async fn save_one_with_diff<F1, F2, F3>(
    pool: &crate::sql::Pool,
    update_query: &crate::core::UpdateQuery,
    before_query: &crate::core::SelectQuery,
    entity_table: &'static str,
    entity_pk: String,
    after_pairs: Vec<(&'static str, serde_json::Value)>,
    decode_before_pg: F1,
    decode_before_my: F2,
    decode_before_sqlite: F3,
) -> Result<u64, crate::sql::ExecError>
where
    F1: FnOnce(&crate::sql::PgReturningRow) -> Vec<(&'static str, serde_json::Value)>,
    F2: FnOnce(&crate::sql::MyReturningRow) -> Vec<(&'static str, serde_json::Value)>,
    F3: FnOnce(&crate::sql::SqliteReturningRow) -> Vec<(&'static str, serde_json::Value)>,
{
    let _ = (&decode_before_pg, &decode_before_my, &decode_before_sqlite);
    update_query.validate()?;
    let stmt = pool.dialect().compile_update(update_query)?;
    let before = pool.dialect().compile_select(before_query)?;
    // Only the pre-update SELECT differs per backend: each row type is a
    // different concrete type, so each arm calls its own
    // `decode_before_*`. The UPDATE, emit and commit are shared by
    // wrapping the transaction in a `PoolTx`.
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let pk_q = before
                .params
                .iter()
                .cloned()
                .fold(sqlx::query(&before.sql), crate::sql::bind_query);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                // A failed pre-read must not let the UPDATE commit unaudited.
                pk_q.fetch_optional(&mut *tx)
                    .await?
                    .map(|row| decode_before_pg(&row));
            let mut wrapped = crate::sql::PoolTx::Postgres(tx);
            let _affected = finish_update_with_audit_diff(
                &mut wrapped,
                Via::Pool(pool),
                &stmt,
                before_pairs,
                &after_pairs,
                entity_table,
                &entity_pk,
            )
            .await?;
            wrapped.commit().await?;
            Ok(_affected)
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut tx = my.begin().await?;
            let pk_q = before
                .params
                .iter()
                .cloned()
                .fold(sqlx::query(&before.sql), crate::sql::bind_query_my);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                // A failed pre-read must not let the UPDATE commit unaudited.
                pk_q.fetch_optional(&mut *tx)
                    .await?
                    .map(|row| decode_before_my(&row));
            let mut wrapped = crate::sql::PoolTx::Mysql(tx);
            let _affected = finish_update_with_audit_diff(
                &mut wrapped,
                Via::Pool(pool),
                &stmt,
                before_pairs,
                &after_pairs,
                entity_table,
                &entity_pk,
            )
            .await?;
            wrapped.commit().await?;
            Ok(_affected)
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut tx = sq.begin().await?;
            let pk_q = before
                .params
                .iter()
                .cloned()
                .fold(sqlx::query(&before.sql), crate::sql::bind_query_sqlite);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                // A failed pre-read must not let the UPDATE commit unaudited.
                pk_q.fetch_optional(&mut *tx)
                    .await?
                    .map(|row| decode_before_sqlite(&row));
            let mut wrapped = crate::sql::PoolTx::Sqlite(tx);
            let _affected = finish_update_with_audit_diff(
                &mut wrapped,
                Via::Pool(pool),
                &stmt,
                before_pairs,
                &after_pairs,
                entity_table,
                &entity_pk,
            )
            .await?;
            wrapped.commit().await?;
            Ok(_affected)
        }
    }
}

/// Shared tail of every [`save_one_with_diff`] arm: run the compiled
/// UPDATE, then emit the audit row if a BEFORE snapshot was captured.
async fn finish_update_with_audit_diff(
    tx: &mut crate::sql::PoolTx<'_>,
    via: Via<'_>,
    stmt: &crate::sql::CompiledStatement,
    before_pairs: Option<Vec<(&'static str, serde_json::Value)>>,
    after_pairs: &[(&'static str, serde_json::Value)],
    entity_table: &'static str,
    entity_pk: &str,
) -> Result<u64, crate::sql::ExecError> {
    // Return rows-affected: 0 means the PK no longer exists.
    let _affected = crate::sql::raw_execute_tx(tx, &stmt.sql, stmt.params.clone()).await?;
    let entry = before_pairs.and_then(|before| {
        PendingEntry::update_diff(entity_table, entity_pk.to_owned(), &before, after_pairs)
    });
    if let Some(entry) = entry {
        emit_one_tx(tx, via, &entry).await?;
    }
    Ok(_affected)
}

#[cfg(test)]
mod tx_update_tests {
    use super::*;

    fn runner<'a>(
        _: &'a crate::sql::Pool,
        _: &'a crate::core::UpdateQuery,
        _: AuditOp,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<u64, crate::sql::ExecError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(0) })
    }

    /// A hand-written entry with a pool runner but no recorder keeps its
    /// audit: no in-transaction update for it (#2010 review).
    #[test]
    fn a_pool_only_audited_update_never_runs_in_a_transaction() {
        use crate::core::{Model as _, ModelEntry};
        let schema = crate::i18n::db::Translation::SCHEMA;
        let audited =
            ModelEntry::new(schema, "app").with_audited(|| Some(runner as AuditedUpdate), || None);
        assert!(TxUpdate::for_entry(Some(&audited)).is_none());
        assert!(TxUpdate::for_entry(Some(&ModelEntry::new(schema, "app"))).is_some());
        assert!(TxUpdate::for_entry(None).is_some());
    }
}
