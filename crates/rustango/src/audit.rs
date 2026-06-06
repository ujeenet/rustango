//! Audit log — single composite-key table that captures every
//! tracked write (insert, update, delete, soft-delete) across every
//! model whose declaration carries `#[rustango(audit(...))]`.
//!
//! The composite key is `(entity_table, entity_pk)` rather than a
//! per-model FK, so one table works for any number of models with
//! different PK shapes (i64, UUID, composite keys stringified). This
//! also keeps the schema flat — operators query `WHERE entity_table =
//! 'post' AND entity_pk = '42'` for a single row's history, and
//! `WHERE entity_table = 'post' ORDER BY occurred_at DESC` for a
//! per-table activity feed.
//!
//! Audit lives **per-tenant** for tenancy projects (the table is
//! created in each tenant's schema/database alongside the app's
//! data) and per-database for stand-alone projects.
//!
//! ## Source of change
//!
//! [`AuditSource`] flows through a tokio task-local so request
//! handlers, seed scripts, and background jobs can declare who's
//! making the write without threading a context object through every
//! ORM call. Default is [`AuditSource::System`]. Per-call override is
//! `Model::save_on_with(conn, source)` — see the macro-generated
//! variants. Admin handlers install the user's session id when the
//! request enters; seed scripts can set `AuditSource::System` (or
//! a custom variant) for their lifetime.
//!
//! ## What gets logged
//!
//! Per-row writes (`save_on`, `insert_on`, `update_on`, `delete_on`,
//! `soft_delete_on`, `restore_on`) capture before/after values for
//! every field listed in the model's `audit(track = "...")`
//! attribute. Bulk variants (`bulk_insert_on`, `bulk_update_on`)
//! batch their entries into a single `INSERT INTO audit_log` after
//! the data write so audit overhead is one extra round-trip even
//! over thousands of rows.

use serde_json::{Map, Value};

use crate::sql::sqlx;

// PG-typed helpers below import PgRow / PgPool / Row directly.
// Sqlite/ MySQL paths use the bi-dialect `ensure_table_pool` /
// `emit_one_pool` further down which dispatch per-backend.
#[cfg(feature = "postgres")]
use crate::sql::sqlx::{postgres::PgRow, PgPool, Row};

/// Source of the change recorded in the audit log.
///
/// `System` is the default (background jobs, seed scripts, framework
/// internals). `User { id }` for authenticated request flows — admin
/// handlers install the session's user id at request entry. `Custom`
/// is a typed escape hatch for project-specific labels (e.g.
/// `"webhook:stripe"`, `"cli:backfill"`).
#[derive(Debug, Clone)]
pub enum AuditSource {
    System,
    User { id: String },
    Custom(String),
}

impl AuditSource {
    /// Stable string representation written to `audit_log.source`.
    /// Used by the macro-emitted insert paths so the on-disk format
    /// stays portable (a downstream search index can join by these
    /// strings without parsing).
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
    /// Task-local audit source. Populated for the duration of an
    /// admin request, a seed closure, etc.; defaults to
    /// [`AuditSource::System`] when no scope has been entered (which
    /// is what `current_source()` returns).
    pub static AUDIT_SOURCE: AuditSource;
}

/// Read the active audit source. Falls back to [`AuditSource::System`]
/// when no [`with_source`] scope is active — matches the "writes from
/// outside any handler are system-attributable" intent.
#[must_use]
pub fn current_source() -> AuditSource {
    AUDIT_SOURCE
        .try_with(Clone::clone)
        .unwrap_or(AuditSource::System)
}

/// Run `fut` with `source` installed as the active audit source. Any
/// audit-emitting ORM call within the future (single-row OR bulk)
/// records `source` on every entry it produces.
///
/// Designed to wrap an admin request handler or a seed-time closure.
/// Outside such a scope, writes record `AuditSource::System`.
pub async fn with_source<F, T>(source: AuditSource, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    AUDIT_SOURCE.scope(source, fut).await
}

/// One pending audit log entry. The macro-generated write paths build
/// these in memory, then [`emit_one`] / [`emit_many`] writes them to
/// the database alongside (or just after) the data write.
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
    /// Non-CRUD operator-side action (e.g. impersonation start /
    /// end, org config edit, branding upload). Used by the operator
    /// console's [`crate::tenancy::operator_console`] audit writes.
    /// (v0.34 — replaces hand-rolled `INSERT INTO rustango_audit_log
    /// … VALUES ('action', …)` SQL.)
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

/// Emit a single entry against a Postgres executor. Used by per-row
/// write paths on PG. For bi-dialect emission see [`emit_one_pool`].
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
              ("entity_table", "entity_pk", "operation", "source", "changes")
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(entry.source.as_token())
    .bind(&entry.changes)
    .execute(executor)
    .await?;
    Ok(())
}

/// Emit a batch of entries in a single Postgres statement. Used by
/// bulk write paths on PG. Sqlite/MySQL fall back to per-row
/// `emit_one_pool` until a bi-dialect batch path lands.
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
    // We compose one big multi-row VALUES list rather than UNNEST-ing
    // 5 typed arrays — keeps the SQL readable and `sqlx` happy with
    // mixed column types (TEXT + JSONB).
    let mut sql = String::from(
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes")
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
            "(${}, ${}, ${}, ${}, ${})",
            bind_idx,
            bind_idx + 1,
            bind_idx + 2,
            bind_idx + 3,
            bind_idx + 4,
        );
        bind_idx += 5;
    }
    let mut q = sqlx::query(&sql);
    for entry in entries {
        q = q
            .bind(entry.entity_table)
            .bind(&entry.entity_pk)
            .bind(entry.operation.as_str())
            .bind(entry.source.as_token())
            .bind(&entry.changes);
    }
    q.execute(executor).await?;
    Ok(())
}

/// Build a `{ "field": { "before": <v>, "after": <v> } }` JSON object
/// from two slices of `(field_name, json_value)` pairs. Skips fields
/// where the before and after values are equal (`update` of a row
/// only logs columns that actually changed).
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

/// Build a `{ "field": <after-value> }` JSON object for create /
/// soft_delete / restore operations where there's no "before" state
/// worth recording.
#[must_use]
pub fn snapshot_changes(after: &[(&str, Value)]) -> Value {
    let mut out = Map::new();
    for (name, val) in after {
        out.insert((*name).to_string(), val.clone());
    }
    Value::Object(out)
}

/// v0.37 — render the audit-log SELECT used by [`fetch_for_entity_pool`]
/// through the framework's dialect emitters. The audit table is
/// framework-owned (not a `#[derive(Model)]`) so it doesn't have a
/// registered `ModelSchema`, but we still want zero hand-rolled SQL
/// in here — `quote_ident` handles backticks-vs-double-quotes and
/// `placeholder` handles `$N`-vs-`?` per dialect.
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

/// v0.37 — render the `DELETE … WHERE occurred_at < $1` used by
/// [`cleanup_older_than_pool`] through the dialect emitter.
fn audit_cleanup_older_than_sql(dialect: &dyn crate::sql::Dialect) -> String {
    let t = dialect.quote_ident("rustango_audit_log");
    let oa = dialect.quote_ident("occurred_at");
    let p1 = dialect.placeholder(1);
    format!("DELETE FROM {t} WHERE {oa} < {p1}")
}

/// v0.37 — render the per-row retention DELETE used by
/// [`cleanup_keep_last_n_pool`]. `ROW_NUMBER() OVER (PARTITION BY)` is
/// supported on PG, MySQL 8+, SQLite 3.25+; only quoting + placeholders
/// vary per dialect.
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

/// Read every audit entry for a given (entity_table, entity_pk)
/// pair, newest first. Convenience for the admin's per-row audit
/// trail panel.
///
/// PG-typed back-compat; for non-PG use [`fetch_for_entity_pool`].
///
/// #562 — delegates to [`fetch_for_entity_pool`] so the SELECT template
/// + decode loop lives in one place. The PG-typed signature stays so
/// older call sites compile unchanged.
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

/// #561 — per-backend AuditEntry row decoders. The `changes`
/// column lives as JSONB on PG (sqlx-postgres decodes straight to
/// `Value`), JSON on MySQL (sqlx-mysql wraps via
/// `sqlx::types::Json<Value>`), and TEXT on SQLite (round-trip
/// through `serde_json::from_str`). Three siblings keep the audit
/// list / fetch arms tight without forcing AuditEntry to implement
/// per-backend `FromRow` blanket impls.
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

/// SQL that creates the `rustango_audit_log` table and its composite
/// `(entity_table, entity_pk)` index. Idempotent (`IF NOT EXISTS`).
/// Mounted by the per-tenant audit bootstrap migration; users with
/// pre-existing rustango deployments can run it directly via
/// `sqlx::query(audit::CREATE_TABLE_SQL).execute(pool)` to retrofit.
pub const CREATE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_audit_log" (
    "id"           BIGSERIAL PRIMARY KEY,
    "entity_table" TEXT NOT NULL,
    "entity_pk"    TEXT NOT NULL,
    "operation"    TEXT NOT NULL,
    "source"       TEXT NOT NULL,
    "changes"      JSONB NOT NULL,
    "occurred_at"  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS "rustango_audit_log_entity_idx"
    ON "rustango_audit_log" ("entity_table", "entity_pk");
CREATE INDEX IF NOT EXISTS "rustango_audit_log_occurred_idx"
    ON "rustango_audit_log" ("occurred_at" DESC);
"#;

/// Delete audit entries older than `cutoff_days` from `pool`'s
/// audit table. Returns the number of rows removed.
///
/// Useful as a retention-policy hook — operators can wire this into
/// a daily cron, a tenant-side maintenance task, or a one-off CLI
/// invocation. Per-tenant scope: each tenant's audit table is its
/// own retention boundary, so `cleanup_older_than(tenant_pool, 90)`
/// expires only that tenant's history. The framework doesn't auto-
/// schedule this — the operator picks the cadence.
///
/// `cutoff_days = 0` clears the entire table (use with caution); a
/// negative value is clamped to 0.
///
/// **PG-only by SQL syntax**: uses `NOW() - ($1::int8 * INTERVAL '1
/// day')` which is Postgres-specific (`INTERVAL` literal + cast
/// syntax). The tri-dialect rewrite computes the cutoff timestamp
/// Rust-side (chrono) and binds it — future work; until then, MySQL/
/// SQLite apps roll their own retention DELETEs.
///
/// # Errors
/// Driver / SQL failures from the DELETE.
#[cfg(feature = "postgres")]
pub async fn cleanup_older_than(pool: &PgPool, cutoff_days: i64) -> Result<u64, sqlx::Error> {
    let cutoff = cutoff_days.max(0);
    let result = sqlx::query(
        r#"DELETE FROM "rustango_audit_log"
           WHERE "occurred_at" < NOW() - ($1::int8 * INTERVAL '1 day')"#,
    )
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Per-row retention: keep the `keep` most recent audit entries
/// per `(entity_table, entity_pk)` pair, deleting the rest. Useful
/// when "the last N revisions of every row" is the right retention
/// shape — e.g. compliance regimes that require keeping the full
/// edit chain but cap storage growth as the table ages.
///
/// Implementation runs a single window-function DELETE: each entry
/// gets a per-row `ROW_NUMBER()` ordered by `occurred_at DESC, id
/// DESC`, and rows with rank > `keep` are dropped. One round-trip
/// regardless of how many `(entity_table, entity_pk)` pairs the
/// table holds.
///
/// `keep = 0` clears the entire table; negative values clamp to 0.
/// Returns the number of rows removed.
///
/// **PG-only by SQL syntax**: uses `ROW_NUMBER() OVER (PARTITION
/// BY …)` which is supported on PG but not uniformly across MySQL
/// (8.0+ only) and not on older SQLite. Future work could emit a
/// per-dialect equivalent; until then, MySQL/SQLite apps implement
/// retention themselves.
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

/// Convenience for tests + ad-hoc setup: ensure the table exists in
/// `pool`'s database / schema. No-op when already present.
///
/// PG-typed back-compat; for non-PG use [`ensure_table_pool`].
///
/// Splits [`CREATE_TABLE_SQL`] on `;` because Postgres' simple-prepare
/// path rejects multiple commands in one prepared statement; each
/// `CREATE TABLE` / `CREATE INDEX` runs as its own round-trip.
///
/// # Errors
/// Driver / SQL failures from `CREATE TABLE IF NOT EXISTS`.
#[cfg(feature = "postgres")]
pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    for stmt in CREATE_TABLE_SQL.split(';') {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        sqlx::query(trimmed).execute(pool).await?;
    }
    Ok(())
}

// ============================================================ bi-dialect audit (v0.23.0-batch16)

/// `MySQL`-shape audit-log DDL. Mirror of [`CREATE_TABLE_SQL`] with
/// MySQL types: `BIGINT AUTO_INCREMENT`, `JSON` (no `JSONB`),
/// `DATETIME(6)` (no `TIMESTAMPTZ`), and backtick identifier quoting
/// since `MySQL`'s parser rejects double-quoted identifiers in
/// default `ANSI_QUOTES=off` mode.
pub const CREATE_TABLE_SQL_MYSQL: &str = r#"
CREATE TABLE IF NOT EXISTS `rustango_audit_log` (
    `id`           BIGINT AUTO_INCREMENT PRIMARY KEY,
    `entity_table` VARCHAR(255) NOT NULL,
    `entity_pk`    VARCHAR(255) NOT NULL,
    `operation`    VARCHAR(32) NOT NULL,
    `source`       VARCHAR(255) NOT NULL,
    `changes`      JSON NOT NULL,
    `occurred_at`  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
);
CREATE INDEX `rustango_audit_log_entity_idx`
    ON `rustango_audit_log` (`entity_table`, `entity_pk`);
CREATE INDEX `rustango_audit_log_occurred_idx`
    ON `rustango_audit_log` (`occurred_at` DESC);
"#;

/// SQLite-shape audit-log DDL. Same column shape as the Postgres
/// version, but: `INTEGER PRIMARY KEY AUTOINCREMENT` (the SQLite
/// auto-PK token), `TEXT` for VARCHAR / JSON / TIMESTAMP (SQLite
/// affinities), `CURRENT_TIMESTAMP` for the default. `CREATE INDEX
/// IF NOT EXISTS` is supported, so the bootstrap stays idempotent
/// without per-error fallback.
pub const CREATE_TABLE_SQL_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_audit_log" (
    "id"           INTEGER PRIMARY KEY AUTOINCREMENT,
    "entity_table" TEXT NOT NULL,
    "entity_pk"    TEXT NOT NULL,
    "operation"    TEXT NOT NULL,
    "source"       TEXT NOT NULL,
    "changes"      TEXT NOT NULL,
    "occurred_at"  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS "rustango_audit_log_entity_idx"
    ON "rustango_audit_log" ("entity_table", "entity_pk");
CREATE INDEX IF NOT EXISTS "rustango_audit_log_occurred_idx"
    ON "rustango_audit_log" ("occurred_at" DESC);
"#;

/// Bootstrap the audit-log table against either backend. Routes the
/// per-dialect DDL through the right driver via [`crate::sql::Pool`].
///
/// `MySQL` caveat: `CREATE INDEX IF NOT EXISTS` doesn't exist in
/// `MySQL`. The bootstrap catches duplicate-index errors (1061) and
/// continues, so the call remains idempotent.
///
/// # Errors
/// Driver / SQL failures other than the swallowed duplicate-index
/// errors on MySQL.
pub async fn ensure_table_pool(pool: &crate::sql::Pool) -> Result<(), sqlx::Error> {
    let ddl = match pool.dialect().name() {
        "postgres" => CREATE_TABLE_SQL,
        "mysql" => CREATE_TABLE_SQL_MYSQL,
        "sqlite" => CREATE_TABLE_SQL_SQLITE,
        // Future dialects fall through to a portable best-effort
        // using `Dialect::column_type` for the timestamp + JSON
        // columns; for the backends rustango ships against, hand-
        // rolled DDL is simpler and produces tighter SQL.
        _ => CREATE_TABLE_SQL,
    };
    // #561 — the split-by-`;` + dispatch + swallow-dup-index loop
    // was duplicated ~6× across audit / media / jobs / contenttypes.
    // Single owner now lives in `crate::sql::run_ddl_idempotent`.
    crate::sql::run_ddl_idempotent(pool, ddl).await
}

/// Per-row audit emit on a `MySqlConnection`-shape executor —
/// counterpart of [`emit_one`] using `?` placeholders + backtick
/// quoting. Used by the macro layer when emitting audited writes
/// over a MySQL transaction.
///
/// # Errors
/// Driver / SQL failures from the INSERT.
#[cfg(feature = "mysql")]
pub async fn emit_one_my<'c, E>(executor: E, entry: &PendingEntry) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::MySql>,
{
    sqlx::query(
        r#"INSERT INTO `rustango_audit_log`
              (`entity_table`, `entity_pk`, `operation`, `source`, `changes`)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(entry.source.as_token())
    .bind(sqlx::types::Json(&entry.changes))
    .execute(executor)
    .await?;
    Ok(())
}

/// SQLite counterpart of [`emit_one`]. Identifier quoting is
/// double-quote (same as Postgres) and placeholders are positional
/// `?` (sqlx-sqlite supports both `?` and `?N`). The `changes` JSON
/// goes into a TEXT column via `sqlx::types::Json`.
///
/// # Errors
/// Driver / SQL failures from the INSERT.
#[cfg(feature = "sqlite")]
pub async fn emit_one_sqlite<'c, E>(executor: E, entry: &PendingEntry) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes")
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(entry.entity_table)
    .bind(&entry.entity_pk)
    .bind(entry.operation.as_str())
    .bind(entry.source.as_token())
    .bind(sqlx::types::Json(&entry.changes))
    .execute(executor)
    .await?;
    Ok(())
}

/// Per-row audit emit via [`crate::sql::Pool`] — dispatches to
/// [`emit_one`] (Postgres) or [`emit_one_my`] (MySQL). **Not
/// transactional** with the data write — for write-and-audit
/// atomicity, acquire a connection / transaction yourself and call
/// the per-backend `emit_one*` directly.
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

/// v0.37 — filter shape for the admin's audit-log activity feed.
/// Each field is optional; `None` means "don't constrain that column".
/// The `list` / `count` helpers turn this into a WHERE clause
/// rendered via [`Dialect::placeholder`] + [`Dialect::quote_ident`].
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub entity_table: Option<String>,
    pub entity_pk: Option<String>,
    pub operation: Option<String>,
    pub source: Option<String>,
}

impl AuditFilter {
    /// Walk the active filters and produce `(column, value)` pairs in
    /// stable order — the order drives placeholder numbering.
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

/// v0.37 — tri-dialect counterpart of the admin audit-log SELECT.
/// Returns a page of `AuditEntry` rows ordered newest-first matching
/// the supplied `AuditFilter`. SQL is rendered through the dialect's
/// emitters; row decode uses the same JSON-bridge logic as
/// [`fetch_for_entity_pool`].
///
/// # Errors
/// Driver / SQL failures from the SELECT, or JSON decode failures on
/// SQLite if the `changes` TEXT column isn't valid JSON.
pub async fn list(
    pool: &crate::sql::Pool,
    filter: &AuditFilter,
    page_size: i64,
    offset: i64,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    let pairs = filter.active_pairs();
    let sql = audit_list_sql(pool.dialect(), &pairs);
    let binds: Vec<&str> = pairs.iter().map(|(_, v)| *v).collect();
    // #561 — was three byte-similar `bind+fetch+decode` arms. The
    // bind+fetch can't share generic code (sqlx::Executor is bound
    // per-Database), but the decode collapses onto the per-backend
    // `AuditEntry::from_*_row` helpers above.
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

/// v0.37 — tri-dialect total count for the admin audit-log pager,
/// honoring the same `AuditFilter` as [`list`].
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
    // #561 — was a 3-arm `match pool` doing the same query_scalar
    // bind-loop per backend. Routes through `raw_query_pool::<(i64,)>`
    // for the single-column COUNT result; the tuple decoder works
    // identically on every backend that has a `FromRow` impl, which
    // includes the bound triple via the `Maybe*FromRow` blanket
    // impls.
    let rows: Vec<(i64,)> = crate::sql::raw_query_pool(&sql, binds, pool)
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })?;
    // COUNT(*) always returns exactly one row.
    Ok(rows.into_iter().next().map_or(0, |t| t.0))
}

/// v0.37 — tri-dialect facet (column, count) groupby for the admin
/// audit-log right rail. Returns rows ordered count-desc, value-asc.
/// SQL is rendered via the dialect emitter — `column` is matched
/// against an allowlist (`entity_table` / `operation` / `source`) to
/// preclude injection.
///
/// # Errors
/// Driver / SQL failures from the SELECT, or
/// `Error::ColumnNotFound` when `column` isn't in the allowlist (the
/// caller has a bug).
pub async fn facet_counts(
    pool: &crate::sql::Pool,
    column: &str,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    // Allowlist guards against injection — the admin handler always
    // passes one of these three, but defense-in-depth is cheap.
    if !matches!(column, "entity_table" | "operation" | "source") {
        return Err(sqlx::Error::ColumnNotFound(column.to_owned()));
    }
    let sql = audit_facet_sql(pool.dialect(), column);
    // #561 — was three byte-identical `try_get("facet_value") + try_get("facet_count")`
    // copies, one per backend. The `(String, i64)` tuple decoder
    // works on every backend via the `Maybe*FromRow` blanket impls;
    // it decodes positionally so it matches the `SELECT … AS facet_value,
    // COUNT(*) AS facet_count` column order in `audit_facet_sql`.
    crate::sql::raw_query_pool::<(String, i64)>(&sql, Vec::new(), pool)
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })
}

/// v0.37 — render the audit-log activity-feed SELECT (paginated, with
/// optional filter pairs) through the dialect's emitters. `pairs`
/// supplies the active filter columns in stable order so placeholder
/// numbering is deterministic.
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

/// v0.37 — `SELECT COUNT(*) FROM rustango_audit_log [WHERE ...]`
/// rendered through the dialect emitter.
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

/// v0.37 — `SELECT col, COUNT(*) FROM rustango_audit_log GROUP BY col
/// ORDER BY count DESC, col` rendered through the dialect emitter.
/// `column` is one of the allowlisted facet columns.
fn audit_facet_sql(dialect: &dyn crate::sql::Dialect, column: &str) -> String {
    let t = dialect.quote_ident("rustango_audit_log");
    let col = dialect.quote_ident(column);
    format!(
        "SELECT {col} AS facet_value, COUNT(*) AS facet_count \
         FROM {t} GROUP BY {col} ORDER BY facet_count DESC, {col}"
    )
}

/// v0.37 — tri-dialect batched audit emit. On Postgres dispatches to
/// the one-statement multi-row [`emit_many`] INSERT; on MySQL/SQLite
/// falls back to a per-row [`emit_one_*`] loop inside a single
/// transaction (one round-trip per row but committed atomically, so
/// admin bulk-action audit rows still all-or-nothing).
///
/// Used by the admin bulk-action handler and by macro-emitted
/// `bulk_*_pool` paths. Empty input returns immediately.
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

/// v0.37 — tri-dialect counterpart of [`fetch_for_entity`]. Decodes
/// rows through the dialect-agnostic `serde_json::Value` bridge so
/// the audit panel renders identically across backends. The `changes`
/// column is JSON-typed on PG/MySQL and TEXT on SQLite — the JSON
/// bridge decodes either shape into `serde_json::Value`.
///
/// # Errors
/// Driver / SQL failures from the SELECT or JSON decode failures
/// (e.g. SQLite TEXT that isn't valid JSON).
pub async fn fetch_for_entity_pool(
    pool: &crate::sql::Pool,
    entity_table: &str,
    entity_pk: &str,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    // Build the SELECT via the dialect's own quoting + placeholder
    // emitters. Same template for every backend — only `quote_ident`
    // ("/`) and `placeholder` ($1 / ?) differ.
    let sql = audit_select_sql(pool.dialect());
    // #561 — was three byte-similar `bind+fetch+decode` arms.
    // Decode collapses onto the per-backend `AuditEntry::from_*_row`
    // helpers (PG/JSONB native, MySQL Json<Value>, SQLite TEXT).
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

/// v0.37 — tri-dialect counterpart of [`cleanup_older_than`]. The
/// cutoff timestamp is computed Rust-side (chrono) and bound as a
/// `TIMESTAMPTZ` / `DATETIME` / ISO-8601 TEXT depending on backend,
/// so the SQL stays portable (no `NOW() - INTERVAL '… day'`).
///
/// `cutoff_days = 0` clears the entire table (use with caution); a
/// negative value is clamped to 0.
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
    // #560 — `occurred_at` is `TEXT DEFAULT CURRENT_TIMESTAMP` on
    // SQLite (`CREATE_TABLE_SQL_SQLITE`). SQLite's CURRENT_TIMESTAMP
    // emits `"YYYY-MM-DD HH:MM:SS"` (space sep, no fractional, no
    // timezone); sqlx-sqlite would otherwise encode a
    // `chrono::DateTime<Utc>` as RFC3339, and lex-compare diverges
    // at position 10 (space < T). Bind the SQLite cutoff in the
    // same CURRENT_TIMESTAMP shape. PG / MySQL keep the native
    // DateTime binding via `SqlValue::DateTime`.
    let bind = if pool.dialect().name() == "sqlite" {
        SqlValue::String(cutoff_ts.format("%Y-%m-%d %H:%M:%S").to_string())
    } else {
        SqlValue::DateTime(cutoff_ts)
    };
    // #561 — was a 3-arm `match pool` that bound per-backend by
    // hand. The bind dispatch already lives in `raw_execute_pool`'s
    // internals — share the same path every other helper uses.
    crate::sql::raw_execute_pool(pool, &sql, vec![bind])
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })
}

/// v0.37 — tri-dialect counterpart of [`cleanup_keep_last_n`].
/// `ROW_NUMBER() OVER (PARTITION BY …)` is supported on PG and on
/// MySQL 8+ / SQLite 3.25+ — the SQL stays the same, only the
/// identifier quoting differs.
///
/// `keep = 0` clears the entire table; negative values clamp to 0.
///
/// # Errors
/// Driver / SQL failures from the DELETE, or an "unsupported window
/// function" error on ancient MySQL 5.7 / SQLite 3.24-. On those
/// backends operators should drop in their own retention DELETE
/// instead of calling this helper.
pub async fn cleanup_keep_last_n_pool(
    pool: &crate::sql::Pool,
    keep: i64,
) -> Result<u64, sqlx::Error> {
    use crate::core::SqlValue;
    let keep = keep.max(0);
    let sql = audit_cleanup_keep_last_n_sql(pool.dialect());
    // #561 — was a 3-arm `match pool` that bound `keep` per-backend
    // by hand. The bind dispatch lives in `raw_execute_pool`.
    crate::sql::raw_execute_pool(pool, &sql, vec![SqlValue::I64(keep)])
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(err) => err,
            other => sqlx::Error::Protocol(format!("{other}")),
        })
}

/// Run `DELETE` from a `DeleteQuery` and emit an audit entry inside
/// a single transaction against either backend. Used by the
/// macro-emitted `Model::delete_pool` for audited models so the data
/// write and the audit row commit atomically — a crash between the
/// two leaves the database consistent (either both rolled back or
/// both committed).
///
/// The DELETE is compiled via `pool.dialect().compile_delete(query)`
/// so identifier quoting + placeholder shape are correct per
/// backend; binding goes through
/// [`crate::sql::executor::bind_query`] / `bind_query_my` (private
/// helpers re-used here through the per-backend arms).
///
/// # Errors
/// Any [`crate::sql::ExecError`] from compile / bind / execute, plus
/// `sqlx::Error` from the audit emit (wrapped as
/// `ExecError::Driver`).
pub async fn delete_one_with_audit(
    pool: &crate::sql::Pool,
    query: &crate::core::DeleteQuery,
    entry: &PendingEntry,
) -> Result<u64, crate::sql::ExecError> {
    let stmt = pool.dialect().compile_delete(query)?;
    // #561 — was a 3-arm `match pool` that opened a per-backend tx,
    // bound stmt.params, executed, called the per-backend
    // `emit_one_<backend>`, committed. The new `raw_execute_tx`
    // combinator (#798) + the `emit_one_tx` shim below let the body
    // collapse to one flat path.
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let affected = crate::sql::raw_execute_tx(&mut tx, &stmt.sql, stmt.params).await?;
    emit_one_tx(&mut tx, entry).await?;
    tx.commit().await?;
    Ok(affected)
}

/// Per-backend dispatch for the audit emit inside an open `PoolTx`.
/// Wraps the existing per-backend `emit_one` / `emit_one_my` /
/// `emit_one_sqlite` helpers (which take a sqlx-typed executor) so
/// callers can stay on the `PoolTx` API instead of unwrapping the
/// variant.
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

/// Run `UPDATE` from an `UpdateQuery` and emit an audit entry inside
/// a single transaction against either backend. Used by the
/// macro-emitted `Model::save_pool` for audited models so the data
/// write and the audit row commit atomically.
///
/// This is a **snapshot-style** audit (the entry's `changes` carries
/// the post-write field values) rather than the diff-style audit the
/// existing `&PgPool` `Model::save` produces. Diff-style audit
/// requires a pre-UPDATE SELECT to capture `before` values per
/// tracked column with their declared Rust types — that's
/// per-model-per-field codegen the macro emits inline today, and
/// porting it to a runtime helper is a separate refactor. Until then,
/// audited writes on `&Pool` lose field-level diff capture but keep
/// post-state provenance.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from compile / bind / execute, plus
/// `sqlx::Error` from the audit emit.
pub async fn save_one_with_audit(
    pool: &crate::sql::Pool,
    query: &crate::core::UpdateQuery,
    entry: &PendingEntry,
) -> Result<u64, crate::sql::ExecError> {
    let stmt = pool.dialect().compile_update(query)?;
    // #561 — same shape as `delete_one_with_audit`; collapses via
    // raw_execute_tx (#798) + emit_one_tx shim.
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let affected = crate::sql::raw_execute_tx(&mut tx, &stmt.sql, stmt.params).await?;
    emit_one_tx(&mut tx, entry).await?;
    tx.commit().await?;
    Ok(affected)
}

/// Run `INSERT` from an `InsertQuery`, capture the auto-assigned PK
/// (PG `RETURNING` row vs MySQL `LAST_INSERT_ID()`), and emit an
/// audit entry inside a single transaction against either backend.
/// Used by the macro-emitted `Model::insert_pool` for audited models.
///
/// Returns [`crate::sql::InsertReturningPool`] — same enum the
/// non-audited [`crate::sql::insert_returning_pool`] returns. The
/// macro-generated caller pattern-matches it to populate the
/// model's `Auto<T>` field (PG arm reads each `returning` column;
/// MySQL arm assigns the single i64).
///
/// MySQL caveat: only a single `Auto<T>` PK can be filled in (one
/// `LAST_INSERT_ID()` value per connection). Multi-Auto-PK models
/// on MySQL surface `SqlError::OperatorNotSupportedInDialect{op:
/// "multi-column RETURNING"}` from the writer when the macro
/// requests >1 returning column — same as the non-audited path.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from compile / bind / execute, plus
/// `sqlx::Error` from the audit emit.
pub async fn insert_one_with_audit(
    pool: &crate::sql::Pool,
    query: &crate::core::InsertQuery,
    entry: &PendingEntry,
) -> Result<crate::sql::InsertReturningPool, crate::sql::ExecError> {
    query.validate()?;
    if query.returning.is_empty() {
        return Err(crate::sql::ExecError::EmptyReturning);
    }
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let stmt = pool.dialect().compile_insert(query)?;
            let mut tx = pg.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_pg(q, v);
            }
            // INSERT … RETURNING — capture the row.
            use sqlx::Executor as _;
            let row = (&mut *tx).fetch_one(q).await?;
            // Update the audit entry's entity_pk to the returned PK
            // when available, so the snapshot's pk reflects the
            // server-assigned value rather than the placeholder.
            // For now we trust the caller-provided entry as-is.
            emit_one(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(crate::sql::InsertReturningPool::PgRow(row))
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            // MySQL has no RETURNING — rewrite to a plain INSERT and
            // read LAST_INSERT_ID() on the same connection.
            let plain = crate::core::InsertQuery {
                model: query.model,
                columns: query.columns.clone(),
                values: query.values.clone(),
                returning: ::std::vec::Vec::new(),
                on_conflict: query.on_conflict.clone(),
            };
            let stmt = pool.dialect().compile_insert(&plain)?;
            let mut tx = my.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_my(q, v);
            }
            q.execute(&mut *tx).await?;
            use sqlx::Row as _;
            let row = sqlx::query("SELECT LAST_INSERT_ID()")
                .fetch_one(&mut *tx)
                .await?;
            let id_u64: u64 = row.try_get::<u64, _>(0)?;
            let id = i64::try_from(id_u64).unwrap_or(i64::MAX);
            emit_one_my(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(crate::sql::InsertReturningPool::MySqlAutoId(id))
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            // SQLite has full RETURNING (≥ 3.35) — same flow as PG.
            let stmt = pool.dialect().compile_insert(query)?;
            let mut tx = sq.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_sqlite(q, v);
            }
            use sqlx::Executor as _;
            let row = (&mut *tx).fetch_one(q).await?;
            emit_one_sqlite(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(crate::sql::InsertReturningPool::SqliteRow(row))
        }
    }
}

/// Local Postgres-typed bind helper — couldn't reuse
/// `executor::bind_query` (it's private to the executor module).
/// Same `bind_match!`-shape body, but copied rather than re-exported
/// to keep the executor surface tight.
///
/// Exposed (under a `__`-prefixed name) so macro-emitted bodies in
/// the audited save_pool diff path (v0.23.0-batch25) can bind
/// `SqlValue` arguments to the per-backend transaction. Not part of
/// the public API.
#[doc(hidden)]
#[cfg(feature = "postgres")]
pub fn __bind_value_pg(
    q: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments> {
    bind_value_pg(q, value)
}

/// MySQL counterpart of [`__bind_value_pg`] — same purpose, MySQL
/// driver type.
#[doc(hidden)]
#[cfg(feature = "mysql")]
pub fn __bind_value_my(
    q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    bind_value_my(q, value)
}

/// SQLite counterpart of [`__bind_value_pg`] — same purpose, SQLite
/// driver type.
#[doc(hidden)]
#[cfg(feature = "sqlite")]
pub fn __bind_value_sqlite<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    bind_value_sqlite(q, value)
}

#[cfg(feature = "postgres")]
fn bind_value_pg(
    q: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments> {
    use crate::core::SqlValue;
    match value {
        SqlValue::Null => q.bind(None::<String>),
        SqlValue::I16(v) => q.bind(v),
        SqlValue::I32(v) => q.bind(v),
        SqlValue::I64(v) => q.bind(v),
        SqlValue::F32(v) => q.bind(v),
        SqlValue::F64(v) => q.bind(v),
        SqlValue::Bool(v) => q.bind(v),
        SqlValue::String(v) => q.bind(v),
        SqlValue::DateTime(v) => q.bind(v),
        SqlValue::Date(v) => q.bind(v),
        SqlValue::Time(v) => q.bind(v),
        SqlValue::Uuid(v) => q.bind(v),
        SqlValue::Json(v) => q.bind(sqlx::types::Json(v)),
        SqlValue::Decimal(v) => q.bind(v),
        SqlValue::Binary(v) => q.bind(v),
        SqlValue::List(_) => unreachable!("List expanded to scalars by SQL writer"),
        // Array values only flow through WHERE clauses, not audit row saves.
        SqlValue::Array(_) => unreachable!("Array values never reach audited-save bind path"),
        SqlValue::RangeLiteral(_) => {
            unreachable!("RangeLiteral values never reach audited-save bind path")
        }
    }
}

#[cfg(feature = "mysql")]
fn bind_value_my(
    q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    use crate::core::SqlValue;
    match value {
        SqlValue::Null => q.bind(None::<String>),
        SqlValue::I16(v) => q.bind(v),
        SqlValue::I32(v) => q.bind(v),
        SqlValue::I64(v) => q.bind(v),
        SqlValue::F32(v) => q.bind(v),
        SqlValue::F64(v) => q.bind(v),
        SqlValue::Bool(v) => q.bind(v),
        SqlValue::String(v) => q.bind(v),
        SqlValue::DateTime(v) => q.bind(v),
        SqlValue::Date(v) => q.bind(v),
        SqlValue::Time(v) => q.bind(v),
        SqlValue::Uuid(v) => q.bind(v),
        SqlValue::Json(v) => q.bind(sqlx::types::Json(v)),
        SqlValue::Decimal(v) => q.bind(v),
        SqlValue::Binary(v) => q.bind(v),
        SqlValue::List(_) => unreachable!("List expanded to scalars by SQL writer"),
        // Array values only flow through WHERE clauses, not audit row saves.
        SqlValue::Array(_) => unreachable!("Array values never reach audited-save bind path"),
        SqlValue::RangeLiteral(_) => {
            unreachable!("RangeLiteral values never reach audited-save bind path")
        }
    }
}

#[cfg(feature = "sqlite")]
fn bind_value_sqlite<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: crate::core::SqlValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    use crate::core::SqlValue;
    match value {
        SqlValue::Null => q.bind(None::<String>),
        SqlValue::I16(v) => q.bind(v),
        SqlValue::I32(v) => q.bind(v),
        SqlValue::I64(v) => q.bind(v),
        SqlValue::F32(v) => q.bind(v),
        SqlValue::F64(v) => q.bind(v),
        SqlValue::Bool(v) => q.bind(v),
        SqlValue::String(v) => q.bind(v),
        SqlValue::DateTime(v) => q.bind(v),
        SqlValue::Date(v) => q.bind(v),
        SqlValue::Time(v) => q.bind(v),
        SqlValue::Uuid(v) => q.bind(v),
        SqlValue::Json(v) => q.bind(sqlx::types::Json(v)),
        // sqlx-sqlite has no `Decimal: Type<Sqlite>` — round-trip via
        // TEXT to match `bind_match_sqlite!` in `sql::executor`.
        SqlValue::Decimal(v) => q.bind(v.to_string()),
        SqlValue::Binary(v) => q.bind(v),
        SqlValue::List(_) => unreachable!("List expanded to scalars by SQL writer"),
        // Array values only flow through WHERE clauses, not audit row saves.
        SqlValue::Array(_) => unreachable!("Array values never reach audited-save bind path"),
        SqlValue::RangeLiteral(_) => {
            unreachable!("RangeLiteral values never reach audited-save bind path")
        }
    }
}

/// Per-row audited save against either backend.
///
/// Slice 17.1 — moved out of the macro into rustango so the
/// `#[cfg(feature = "postgres")]` / `#[cfg(feature = "mysql")]`
/// arms no longer leak into consumer-crate macro expansions.
///
/// Steps inside one transaction:
/// 1. Run the per-backend BEFORE-snapshot SELECT and decode tracked
///    columns into `(col, json)` pairs via `decode_before_pg` /
///    `decode_before_my`.
/// 2. Execute the compiled UPDATE.
/// 3. Build AFTER pairs via `after_pairs` and diff against BEFORE.
/// 4. Emit an `Update` audit entry on the same transaction.
/// 5. Commit.
///
/// Closure types reference [`crate::sql::PgReturningRow`] /
/// [`crate::sql::MyReturningRow`] aliases, which resolve to
/// uninhabited types when the matching feature is off — keeps
/// macro-emitted closure bodies typecheckable in any feature config.
///
/// # Errors
/// Any [`crate::sql::ExecError`] from the UPDATE/SELECT, plus
/// `sqlx::Error` from the audit emit (mapped through `From`).
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
) -> Result<(), crate::sql::ExecError>
where
    F1: FnOnce(&crate::sql::PgReturningRow) -> Vec<(&'static str, serde_json::Value)>,
    F2: FnOnce(&crate::sql::MyReturningRow) -> Vec<(&'static str, serde_json::Value)>,
    F3: FnOnce(&crate::sql::SqliteReturningRow) -> Vec<(&'static str, serde_json::Value)>,
{
    let _ = (&decode_before_pg, &decode_before_my, &decode_before_sqlite);
    let _ = (select_cols_pg, select_cols_my, select_cols_sqlite);
    let stmt = pool.dialect().compile_update(update_query)?;
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let select_sql = format!(
                r#"SELECT {} FROM "{}" WHERE "{}" = $1"#,
                select_cols_pg, entity_table, pk_column,
            );
            let pk_q = sqlx::query(&select_sql);
            let pk_q = bind_value_pg(pk_q, pk_value);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                match pk_q.fetch_optional(&mut *tx).await {
                    Ok(Some(row)) => Some(decode_before_pg(&row)),
                    _ => None,
                };
            let mut q = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_pg(q, v);
            }
            q.execute(&mut *tx).await?;
            if let Some(before) = before_pairs {
                let entry = PendingEntry {
                    entity_table,
                    entity_pk,
                    operation: AuditOp::Update,
                    source: current_source(),
                    changes: diff_changes(&before, &after_pairs),
                };
                emit_one(&mut *tx, &entry).await?;
            }
            tx.commit().await?;
            Ok(())
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut tx = my.begin().await?;
            let select_sql = format!(
                "SELECT {} FROM `{}` WHERE `{}` = ?",
                select_cols_my, entity_table, pk_column,
            );
            let pk_q = sqlx::query(&select_sql);
            let pk_q = bind_value_my(pk_q, pk_value);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                match pk_q.fetch_optional(&mut *tx).await {
                    Ok(Some(row)) => Some(decode_before_my(&row)),
                    _ => None,
                };
            let mut q = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_my(q, v);
            }
            q.execute(&mut *tx).await?;
            if let Some(before) = before_pairs {
                let entry = PendingEntry {
                    entity_table,
                    entity_pk,
                    operation: AuditOp::Update,
                    source: current_source(),
                    changes: diff_changes(&before, &after_pairs),
                };
                emit_one_my(&mut *tx, &entry).await?;
            }
            tx.commit().await?;
            Ok(())
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut tx = sq.begin().await?;
            let select_sql = format!(
                r#"SELECT {} FROM "{}" WHERE "{}" = ?"#,
                select_cols_sqlite, entity_table, pk_column,
            );
            let pk_q = sqlx::query(&select_sql);
            let pk_q = bind_value_sqlite(pk_q, pk_value);
            let before_pairs: Option<Vec<(&'static str, serde_json::Value)>> =
                match pk_q.fetch_optional(&mut *tx).await {
                    Ok(Some(row)) => Some(decode_before_sqlite(&row)),
                    _ => None,
                };
            let mut q = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_sqlite(q, v);
            }
            q.execute(&mut *tx).await?;
            if let Some(before) = before_pairs {
                let entry = PendingEntry {
                    entity_table,
                    entity_pk,
                    operation: AuditOp::Update,
                    source: current_source(),
                    changes: diff_changes(&before, &after_pairs),
                };
                emit_one_sqlite(&mut *tx, &entry).await?;
            }
            tx.commit().await?;
            Ok(())
        }
    }
}
