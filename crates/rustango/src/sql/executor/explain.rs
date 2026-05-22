//! `EXPLAIN` plan dump — `ExplainOptions` + `ExplainFormat` +
//! `explain_pool` tri-dialect helper. Issue #272 / T1.10.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 5. The
//! `QuerySet::explain` / `QuerySet::explain_on` inherent methods
//! still live in mod.rs since they're impls on the QuerySet inherent
//! surface; they consume `ExplainOptions` / `ExplainFormat` from here.

#[cfg(feature = "postgres")]
use sqlx::postgres::PgArguments;
#[cfg(feature = "postgres")]
use sqlx::query::Query;

#[cfg(feature = "postgres")]
use super::bind_query;
#[cfg(feature = "mysql")]
use super::bind_query_my;
#[cfg(feature = "sqlite")]
use super::bind_query_sqlite;
use super::ExecError;
use crate::core::SelectQuery;
use crate::sql::Pool;

/// Knobs for [`QuerySet::explain_on`]. Defaults render plain
/// `EXPLAIN <stmt>` — safe to call (no execution, no side effects).
/// Opt into `analyze` / `buffers` / `format = ExplainFormat::Json`
/// for richer output; `ANALYZE` actually runs the query so it
/// reflects real I/O + timing.
#[derive(Debug, Clone, Default)]
pub struct ExplainOptions {
    /// `EXPLAIN (ANALYZE)` — actually runs the query and reports
    /// real timings + row counts. Off by default; turning it on for
    /// a write query (UPDATE/DELETE — not currently exposed via
    /// QuerySet::explain) would mutate state.
    pub analyze: bool,
    /// `EXPLAIN (BUFFERS)` — reports cache hit / disk read counts.
    /// Requires `analyze = true` to surface buffer numbers.
    pub buffers: bool,
    /// `EXPLAIN (VERBOSE)` — adds output column lists and schema-
    /// qualified table names.
    pub verbose: bool,
    /// Output format. Default = text. JSON is parseable; YAML/XML
    /// are also Postgres-supported but rarely useful here.
    pub format: ExplainFormat,
}

/// Output format selector for [`ExplainOptions::format`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExplainFormat {
    #[default]
    Text,
    Json,
    Yaml,
    Xml,
}

#[cfg(feature = "postgres")]
impl ExplainOptions {
    /// Render the parenthesized option list (e.g. `(ANALYZE,
    /// BUFFERS, FORMAT JSON)`). Empty when every option is at its
    /// default. Used by both the PG-typed `QuerySet::explain_on` and
    /// the tri-dialect [`explain_pool`] PG branch — the latter
    /// inherits the same `postgres`-feature gating so MySQL / SQLite
    /// builds don't drag this in.
    pub(super) fn to_clause(&self) -> String {
        let mut bits: Vec<&'static str> = Vec::new();
        if self.analyze {
            bits.push("ANALYZE");
        }
        if self.buffers {
            bits.push("BUFFERS");
        }
        if self.verbose {
            bits.push("VERBOSE");
        }
        let format_bit = match self.format {
            ExplainFormat::Text => None,
            ExplainFormat::Json => Some("FORMAT JSON"),
            ExplainFormat::Yaml => Some("FORMAT YAML"),
            ExplainFormat::Xml => Some("FORMAT XML"),
        };
        if let Some(f) = format_bit {
            bits.push(f);
        }
        if bits.is_empty() {
            String::new()
        } else {
            format!("({})", bits.join(", "))
        }
    }
}

/// Tri-dialect query-plan helper — issue #272 / T1.10.
///
/// Routes to the active backend's `EXPLAIN` syntax via [`Pool::dialect`]
/// and returns a single newline-joined string suitable for printing.
///
/// Per-backend behavior:
///
/// * **Postgres** — emits `EXPLAIN (FORMAT TEXT|JSON|YAML|XML[, ANALYZE,
///   BUFFERS, VERBOSE]) <stmt>`. Each row's first column joined by
///   newline. JSON cells round-trip through [`serde_json::Value`].
///
/// * **MySQL** — emits one of:
///   * `EXPLAIN ANALYZE <stmt>` when `options.analyze == true` (8.0.18+).
///     Returns a single `EXPLAIN` column as TEXT.
///   * `EXPLAIN FORMAT=JSON <stmt>` when `options.format ==
///     ExplainFormat::Json` (single JSON column).
///   * `EXPLAIN FORMAT=TREE <stmt>` otherwise (8.0.16+). Returns one
///     TEXT column with the optimizer's structured plan.
///
///   MySQL's classic tabular `EXPLAIN <stmt>` (multi-column) is not
///   emitted by this helper because the single-string return shape
///   doesn't fit. Use `raw_query_pool` if you need the tabular form.
///
/// * **SQLite** — emits `EXPLAIN QUERY PLAN <stmt>`. Plan-only — never
///   executes the query, so `analyze` / `buffers` flags silently no-op.
///   For `ExplainFormat::Text` each row's `detail` column is joined by
///   newline. For `ExplainFormat::Json` the rows are serialized as
///   `[{id, parent, detail}, ...]`.
///
/// Flags that don't apply to a backend silently no-op — the call still
/// succeeds. The query is bound through the same parameter binding the
/// rest of the `_pool` family uses, so user-supplied filter values are
/// safe even with `analyze = true`.
///
/// # Errors
/// SQL-writing or driver failures from the EXPLAIN.
pub async fn explain_pool(
    pool: &Pool,
    query: &SelectQuery,
    options: ExplainOptions,
) -> Result<String, ExecError> {
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut sql = String::with_capacity(stmt.sql.len() + 32);
            sql.push_str("EXPLAIN ");
            let clause = options.to_clause();
            if !clause.is_empty() {
                sql.push_str(&clause);
                sql.push(' ');
            }
            sql.push_str(&stmt.sql);
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            let mut parts = Vec::with_capacity(rows.len());
            for row in &rows {
                let line: String = match options.format {
                    ExplainFormat::Json => {
                        let v: serde_json::Value = sqlx::Row::try_get(row, 0)?;
                        v.to_string()
                    }
                    ExplainFormat::Text | ExplainFormat::Yaml | ExplainFormat::Xml => {
                        sqlx::Row::try_get(row, 0)?
                    }
                };
                parts.push(line);
            }
            Ok(parts.join("\n"))
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            // MySQL's `EXPLAIN` keyword family is unique per shape:
            // `EXPLAIN ANALYZE` (8.0.18+) actually runs the query and
            // reports real timings; `EXPLAIN FORMAT=JSON|TREE` is plan-
            // only. `analyze` wins over `format` — ANALYZE implicitly
            // uses TREE format on text and JSON on `FORMAT=JSON` was
            // never standardized for ANALYZE in MySQL 8.x.
            let prefix = if options.analyze {
                "EXPLAIN ANALYZE "
            } else {
                match options.format {
                    ExplainFormat::Json => "EXPLAIN FORMAT=JSON ",
                    _ => "EXPLAIN FORMAT=TREE ",
                }
            };
            let mut sql = String::with_capacity(stmt.sql.len() + prefix.len());
            sql.push_str(prefix);
            sql.push_str(&stmt.sql);
            let mut q = sqlx::query(&sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            let mut parts = Vec::with_capacity(rows.len());
            for row in &rows {
                let line: String = match options.format {
                    ExplainFormat::Json if !options.analyze => {
                        let v: serde_json::Value = sqlx::Row::try_get(row, 0)?;
                        v.to_string()
                    }
                    _ => sqlx::Row::try_get(row, 0)?,
                };
                parts.push(line);
            }
            Ok(parts.join("\n"))
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            // SQLite's EXPLAIN QUERY PLAN is plan-only (never runs the
            // query), so analyze/buffers/verbose flags silently no-op
            // per the issue spec. Returns 4 columns: id, parent, notused,
            // detail.
            let sql = format!("EXPLAIN QUERY PLAN {}", stmt.sql);
            let mut q = sqlx::query(&sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            match options.format {
                ExplainFormat::Json => {
                    let mut arr = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let id: i64 = sqlx::Row::try_get(row, 0)?;
                        let parent: i64 = sqlx::Row::try_get(row, 1)?;
                        let detail: String = sqlx::Row::try_get(row, 3)?;
                        arr.push(serde_json::json!({
                            "id": id,
                            "parent": parent,
                            "detail": detail,
                        }));
                    }
                    Ok(serde_json::Value::Array(arr).to_string())
                }
                ExplainFormat::Text | ExplainFormat::Yaml | ExplainFormat::Xml => {
                    let mut parts = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let detail: String = sqlx::Row::try_get(row, 3)?;
                        parts.push(detail);
                    }
                    Ok(parts.join("\n"))
                }
            }
        }
    }
}
