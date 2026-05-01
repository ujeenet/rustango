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

use crate::sql::sqlx::{self, postgres::PgRow, PgPool, Row};

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
        }
    }
}

/// Emit a single entry. Used by per-row write paths.
///
/// # Errors
/// Driver / SQL failures from the INSERT.
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

/// Emit a batch of entries in a single statement. Used by bulk write
/// paths so audit overhead is one extra round-trip even when the
/// underlying write affected N rows.
///
/// # Errors
/// As [`emit_one`].
pub async fn emit_many<'c, E>(
    executor: E,
    entries: &[PendingEntry],
) -> Result<(), sqlx::Error>
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
/// # Errors
/// Driver / SQL failures.
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

/// Convenience for tests + ad-hoc setup: ensure the table exists in
/// `pool`'s database / schema. No-op when already present.
///
/// Splits [`CREATE_TABLE_SQL`] on `;` because Postgres' simple-prepare
/// path rejects multiple commands in one prepared statement; each
/// `CREATE TABLE` / `CREATE INDEX` runs as its own round-trip.
///
/// # Errors
/// Driver / SQL failures from `CREATE TABLE IF NOT EXISTS`.
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
