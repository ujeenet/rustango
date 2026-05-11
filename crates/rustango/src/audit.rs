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

/// Read every audit entry for a given (entity_table, entity_pk)
/// pair, newest first. Convenience for the admin's per-row audit
/// trail panel.
///
/// PG-typed back-compat; for non-PG use [`fetch_for_entity_pool`].
///
/// # Errors
/// Driver / SQL failures.
#[cfg(feature = "postgres")]
pub async fn fetch_for_entity(
    pool: &PgPool,
    entity_table: &str,
    entity_pk: &str,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    let rows: Vec<PgRow> = sqlx::query(
        r#"SELECT "id", "entity_table", "entity_pk", "operation",
                  "source", "changes", "occurred_at"
           FROM "rustango_audit_log"
           WHERE "entity_table" = $1 AND "entity_pk" = $2
           ORDER BY "occurred_at" DESC, "id" DESC"#,
    )
    .bind(entity_table)
    .bind(entity_pk)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(AuditEntry::from_row(&row)?);
    }
    Ok(out)
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
    let dialect = pool.dialect();
    let ddl = match dialect.name() {
        "postgres" => CREATE_TABLE_SQL,
        "mysql" => CREATE_TABLE_SQL_MYSQL,
        "sqlite" => CREATE_TABLE_SQL_SQLITE,
        // Future dialects fall through to a portable best-effort using
        // `Dialect::column_type` for the timestamp + JSON columns; for
        // the backends rustango ships against, hand-rolled DDL is
        // simpler and produces tighter SQL.
        _ => CREATE_TABLE_SQL,
    };
    for stmt in ddl.split(';') {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        match pool {
            #[cfg(feature = "postgres")]
            crate::sql::Pool::Postgres(pg) => {
                sqlx::query(trimmed).execute(pg).await?;
            }
            #[cfg(feature = "mysql")]
            crate::sql::Pool::Mysql(my) => {
                if let Err(e) = sqlx::query(trimmed).execute(my).await {
                    // MySQL has no CREATE INDEX IF NOT EXISTS — the
                    // index-create statements raise error 1061 when
                    // the index already exists. Swallow that one
                    // case so the bootstrap stays idempotent;
                    // surface every other error.
                    if !is_mysql_dup_index_error(&e) {
                        return Err(e);
                    }
                }
            }
            #[cfg(feature = "sqlite")]
            crate::sql::Pool::Sqlite(sq) => {
                sqlx::query(trimmed).execute(sq).await?;
            }
        }
    }
    Ok(())
}

#[cfg(feature = "mysql")]
fn is_mysql_dup_index_error(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db) = e {
        return db.code().as_deref() == Some("42000")
            || db.message().contains("Duplicate key name");
    }
    false
}

#[cfg(not(feature = "mysql"))]
#[allow(dead_code)]
fn is_mysql_dup_index_error(_e: &sqlx::Error) -> bool {
    false
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
pub async fn delete_one_with_audit_pool(
    pool: &crate::sql::Pool,
    query: &crate::core::DeleteQuery,
    entry: &PendingEntry,
) -> Result<u64, crate::sql::ExecError> {
    let stmt = pool.dialect().compile_delete(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_pg(q, v);
            }
            let affected = q.execute(&mut *tx).await?.rows_affected();
            emit_one(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(affected)
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut tx = my.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_my(q, v);
            }
            let affected = q.execute(&mut *tx).await?.rows_affected();
            emit_one_my(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(affected)
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut tx = sq.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_sqlite(q, v);
            }
            let affected = q.execute(&mut *tx).await?.rows_affected();
            emit_one_sqlite(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(affected)
        }
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
pub async fn save_one_with_audit_pool(
    pool: &crate::sql::Pool,
    query: &crate::core::UpdateQuery,
    entry: &PendingEntry,
) -> Result<u64, crate::sql::ExecError> {
    let stmt = pool.dialect().compile_update(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut tx = pg.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_pg(q, v);
            }
            let affected = q.execute(&mut *tx).await?.rows_affected();
            emit_one(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(affected)
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut tx = my.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_my(q, v);
            }
            let affected = q.execute(&mut *tx).await?.rows_affected();
            emit_one_my(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(affected)
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut tx = sq.begin().await?;
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_value_sqlite(q, v);
            }
            let affected = q.execute(&mut *tx).await?.rows_affected();
            emit_one_sqlite(&mut *tx, entry).await?;
            tx.commit().await?;
            Ok(affected)
        }
    }
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
pub async fn insert_one_with_audit_pool(
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
        SqlValue::Uuid(v) => q.bind(v),
        SqlValue::Json(v) => q.bind(sqlx::types::Json(v)),
        SqlValue::List(_) => unreachable!("List expanded to scalars by SQL writer"),
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
        SqlValue::Uuid(v) => q.bind(v),
        SqlValue::Json(v) => q.bind(sqlx::types::Json(v)),
        SqlValue::List(_) => unreachable!("List expanded to scalars by SQL writer"),
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
        SqlValue::Uuid(v) => q.bind(v),
        SqlValue::Json(v) => q.bind(sqlx::types::Json(v)),
        SqlValue::List(_) => unreachable!("List expanded to scalars by SQL writer"),
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
pub async fn save_one_with_diff_pool<F1, F2, F3>(
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
