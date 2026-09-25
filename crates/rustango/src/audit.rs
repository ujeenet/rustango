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
//! insert them in one statement after the data write, so the cost stays at
//! one extra round-trip even for thousands of rows.

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

/// Read the active audit source. Returns [`AuditSource::System`] when no
/// [`with_source`] scope is active.
#[must_use]
pub fn current_source() -> AuditSource {
    AUDIT_SOURCE
        .try_with(Clone::clone)
        .unwrap_or(AuditSource::System)
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
    AUDIT_SOURCE.scope(source, fut).await
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
    sqlx::query(
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes", "occurred_at")
           VALUES ($1, $2, $3, $4, $5, $6)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(entry.source.as_token())
    .bind(&entry.changes)
    .bind(chrono::Utc::now())
    .execute(executor)
    .await?;
    Ok(())
}

/// Emit a batch of entries in one Postgres statement. Used by bulk write
/// paths on PG. SQLite and MySQL loop per row inside a transaction — see
/// [`emit_many_pool`].
///
/// # Errors
/// As [`emit_one`].
#[cfg(feature = "postgres")]
pub async fn emit_many<'c, E>(executor: E, entries: &[PendingEntry]) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    if entries.is_empty() {
        return Ok(());
    }
    // One multi-row VALUES list, not six UNNEST-ed typed arrays: simpler
    // SQL, and sqlx handles the mixed TEXT + JSONB + TIMESTAMPTZ columns.
    let mut sql = String::from(
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes", "occurred_at")
           VALUES "#,
    );
    let mut bind_idx = 1usize;
    for (i, _) in entries.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        use std::fmt::Write as _;
        let _ = write!(
            sql,
            "(${}, ${}, ${}, ${}, ${}, ${})",
            bind_idx,
            bind_idx + 1,
            bind_idx + 2,
            bind_idx + 3,
            bind_idx + 4,
            bind_idx + 5,
        );
        bind_idx += 6;
    }
    let mut q = sqlx::query(&sql);
    for entry in entries {
        // Stamped per row, not once per batch, so this matches the
        // MySQL / SQLite fallback, which loops `emit_one_*`.
        q = q
            .bind(entry.entity_table)
            .bind(&entry.entity_pk)
            .bind(entry.operation.as_str())
            .bind(entry.source.as_token())
            .bind(&entry.changes)
            .bind(chrono::Utc::now());
    }
    q.execute(executor).await?;
    Ok(())
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
    let changes =
        crate::migrate::detect_changes(&crate::migrate::SchemaSnapshot::default(), &snapshot);
    let batch =
        crate::migrate::render_changes_split_with_dialect(&changes, &snapshot, pool.dialect())
            .map_err(sqlx::Error::Protocol)?;
    crate::migrate::apply_idempotent(pool, &batch).await?;
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
    // `occurred_at` is bound, not defaulted — see `emit_one`.
    sqlx::query(
        r#"INSERT INTO `rustango_audit_log`
              (`entity_table`, `entity_pk`, `operation`, `source`, `changes`, `occurred_at`)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(entry.source.as_token())
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
    .bind(entry.source.as_token())
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
        crate::sql::Pool::Postgres(pg) => emit_one(pg, entry).await,
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => emit_one_my(my, entry).await,
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => emit_one_sqlite(sq, entry).await,
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
    let pairs = filter.active_pairs();
    let sql = audit_list_sql(pool.dialect(), &pairs);
    let binds: Vec<&str> = pairs.iter().map(|(_, v)| *v).collect();
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
    use crate::core::SqlValue;
    let pairs = filter.active_pairs();
    let sql = audit_count_sql(pool.dialect(), &pairs);
    let binds: Vec<SqlValue> = pairs
        .iter()
        .map(|(_, v)| SqlValue::String((*v).to_owned()))
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
    // `column` is interpolated into the SQL, so it must be allowlisted.
    if !matches!(column, "entity_table" | "operation" | "source") {
        return Err(sqlx::Error::ColumnNotFound(column.to_owned()));
    }
    let sql = audit_facet_sql(pool.dialect(), column);
    // The `(String, i64)` tuple decodes positionally, so it matches the
    // `facet_value, facet_count` column order in `audit_facet_sql`.
    crate::sql::raw_query_pool::<(String, i64)>(&sql, Vec::new(), pool)
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })
}

/// Render the paginated activity-feed SELECT. `pairs` gives the active
/// filter columns in a stable order, so placeholder numbering is fixed.
fn audit_list_sql(dialect: &dyn crate::sql::Dialect, pairs: &[(&'static str, &str)]) -> String {
    use std::fmt::Write as _;
    let t = dialect.quote_ident("rustango_audit_log");
    let id = dialect.quote_ident("id");
    let et = dialect.quote_ident("entity_table");
    let ek = dialect.quote_ident("entity_pk");
    let op = dialect.quote_ident("operation");
    let src = dialect.quote_ident("source");
    let ch = dialect.quote_ident("changes");
    let oa = dialect.quote_ident("occurred_at");
    let mut sql = String::new();
    let _ = write!(
        sql,
        "SELECT {id}, {et}, {ek}, {op}, {src}, {ch}, {oa} FROM {t}",
    );
    let mut bind_idx = 1usize;
    for (i, (col, _)) in pairs.iter().enumerate() {
        let prefix = if i == 0 { " WHERE " } else { " AND " };
        let col_q = dialect.quote_ident(col);
        let ph = dialect.placeholder(bind_idx);
        let _ = write!(sql, "{prefix}{col_q} = {ph}");
        bind_idx += 1;
    }
    let p_limit = dialect.placeholder(bind_idx);
    let p_offset = dialect.placeholder(bind_idx + 1);
    let _ = write!(
        sql,
        " ORDER BY {oa} DESC, {id} DESC LIMIT {p_limit} OFFSET {p_offset}"
    );
    sql
}

/// Render `SELECT COUNT(*) FROM rustango_audit_log [WHERE ...]`.
fn audit_count_sql(dialect: &dyn crate::sql::Dialect, pairs: &[(&'static str, &str)]) -> String {
    use std::fmt::Write as _;
    let t = dialect.quote_ident("rustango_audit_log");
    let mut sql = format!("SELECT COUNT(*) FROM {t}");
    for (i, (col, _)) in pairs.iter().enumerate() {
        let prefix = if i == 0 { " WHERE " } else { " AND " };
        let col_q = dialect.quote_ident(col);
        let ph = dialect.placeholder(i + 1);
        let _ = write!(sql, "{prefix}{col_q} = {ph}");
    }
    sql
}

/// Render the facet group-by. `column` must already be allowlisted by
/// [`facet_counts`].
fn audit_facet_sql(dialect: &dyn crate::sql::Dialect, column: &str) -> String {
    let t = dialect.quote_ident("rustango_audit_log");
    let col = dialect.quote_ident(column);
    format!(
        "SELECT {col} AS facet_value, COUNT(*) AS facet_count \
         FROM {t} GROUP BY {col} ORDER BY facet_count DESC, {col}"
    )
}

/// Batched audit emit on any backend. Postgres uses the one-statement
/// [`emit_many`] INSERT. MySQL and SQLite loop per row inside one
/// transaction: a round-trip per row, but still all-or-nothing.
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
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => emit_many(pg, entries).await,
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut tx = my.begin().await?;
            for entry in entries {
                emit_one_my(&mut *tx, entry).await?;
            }
            tx.commit().await
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut tx = sq.begin().await?;
            for entry in entries {
                emit_one_sqlite(&mut *tx, entry).await?;
            }
            tx.commit().await
        }
    }
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
/// the row and its audit record commit together. Used by the generated
/// `Model::delete_pool` for audited models.
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
    emit_one_tx(&mut tx, entry).await?;
    tx.commit().await?;
    Ok(affected)
}

/// Emit an audit entry inside an open `PoolTx`, so callers stay on the
/// `PoolTx` API instead of unwrapping the backend variant themselves.
async fn emit_one_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    entry: &PendingEntry,
) -> Result<(), sqlx::Error> {
    match tx {
        #[cfg(feature = "postgres")]
        crate::sql::PoolTx::Postgres(t) => emit_one(&mut **t, entry).await,
        #[cfg(feature = "mysql")]
        crate::sql::PoolTx::Mysql(t) => emit_one_my(&mut **t, entry).await,
        #[cfg(feature = "sqlite")]
        crate::sql::PoolTx::Sqlite(t) => emit_one_sqlite(&mut **t, entry).await,
    }
}

/// Run an `UpdateQuery` and emit its audit entry in one transaction.
/// Used by the generated `Model::save_pool` for audited models.
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
    let stmt = pool.dialect().compile_update(query)?;
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let affected = crate::sql::raw_execute_tx(&mut tx, &stmt.sql, stmt.params).await?;
    emit_one_tx(&mut tx, entry).await?;
    tx.commit().await?;
    Ok(affected)
}

/// Run an `InsertQuery`, capture the auto-assigned PK, and emit the audit
/// entry in one transaction. Used by the generated `Model::insert_pool`
/// for audited models.
///
/// Returns the same [`crate::sql::InsertReturningPool`] as the
/// non-audited [`crate::sql::insert_returning_pool`].
///
/// MySQL fills in only one `Auto<T>` PK, because a connection has a
/// single `LAST_INSERT_ID()`. A model with more than one returns
/// `SqlError::OperatorNotSupportedInDialect`, as on the non-audited path.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from compile, bind or execute, plus
/// `sqlx::Error` from the audit emit.
pub async fn insert_one_with_audit(
    pool: &crate::sql::Pool,
    query: &crate::core::InsertQuery,
    entry: &PendingEntry,
) -> Result<crate::sql::InsertReturningPool, crate::sql::ExecError> {
    // `insert_returning_tx` already handles each backend's return shape.
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let returning = crate::sql::insert_returning_tx(&mut tx, query).await?;
    emit_one_tx(&mut tx, entry).await?;
    tx.commit().await?;
    Ok(returning)
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
    pk_column: &'static str,
    pk_value: crate::core::SqlValue,
    entity_table: &'static str,
    entity_pk: String,
    after_pairs: Vec<(&'static str, serde_json::Value)>,
    select_cols_pg: &str,
    select_cols_my: &str,
    select_cols_sqlite: &str,
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
    let _ = (select_cols_pg, select_cols_my, select_cols_sqlite);
    let stmt = pool.dialect().compile_update(update_query)?;
    // Only the pre-update SELECT differs per backend: each row type is a
    // different concrete type, so each arm calls its own
    // `decode_before_*`. The UPDATE, emit and commit are shared by
    // wrapping the transaction in a `PoolTx`.
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let select_sql = format!(
                r#"SELECT {} FROM "{}" WHERE "{}" = $1"#,
                select_cols_pg, entity_table, pk_column,
            );
            let pk_q = sqlx::query(&select_sql);
            let pk_q = crate::sql::bind_query(pk_q, pk_value);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                match pk_q.fetch_optional(&mut *tx).await {
                    Ok(Some(row)) => Some(decode_before_pg(&row)),
                    _ => None,
                };
            let mut wrapped = crate::sql::PoolTx::Postgres(tx);
            let _affected = finish_update_with_audit_diff(
                &mut wrapped,
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
            let select_sql = format!(
                "SELECT {} FROM `{}` WHERE `{}` = ?",
                select_cols_my, entity_table, pk_column,
            );
            let pk_q = sqlx::query(&select_sql);
            let pk_q = crate::sql::bind_query_my(pk_q, pk_value);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                match pk_q.fetch_optional(&mut *tx).await {
                    Ok(Some(row)) => Some(decode_before_my(&row)),
                    _ => None,
                };
            let mut wrapped = crate::sql::PoolTx::Mysql(tx);
            let _affected = finish_update_with_audit_diff(
                &mut wrapped,
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
            let select_sql = format!(
                r#"SELECT {} FROM "{}" WHERE "{}" = ?"#,
                select_cols_sqlite, entity_table, pk_column,
            );
            let pk_q = sqlx::query(&select_sql);
            let pk_q = crate::sql::bind_query_sqlite(pk_q, pk_value);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                match pk_q.fetch_optional(&mut *tx).await {
                    Ok(Some(row)) => Some(decode_before_sqlite(&row)),
                    _ => None,
                };
            let mut wrapped = crate::sql::PoolTx::Sqlite(tx);
            let _affected = finish_update_with_audit_diff(
                &mut wrapped,
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
    stmt: &crate::sql::CompiledStatement,
    before_pairs: Option<Vec<(&'static str, serde_json::Value)>>,
    after_pairs: &[(&'static str, serde_json::Value)],
    entity_table: &'static str,
    entity_pk: &str,
) -> Result<u64, crate::sql::ExecError> {
    // Return rows-affected: 0 means the PK no longer exists.
    let _affected = crate::sql::raw_execute_tx(tx, &stmt.sql, stmt.params.clone()).await?;
    if let Some(before) = before_pairs {
        let entry = PendingEntry {
            entity_table,
            entity_pk: entity_pk.to_owned(),
            operation: AuditOp::Update,
            source: current_source(),
            changes: diff_changes(&before, after_pairs),
        };
        emit_one_tx(tx, &entry).await?;
    }
    Ok(_affected)
}
