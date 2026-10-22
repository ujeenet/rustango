//! `EXPLAIN` plan dump — `ExplainOptions` + `ExplainFormat` +
//! `explain_pool` tri-dialect helper. Issue #272 / T1.10.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 5. The
//! `QuerySet::explain` / `QuerySet::explain_on` inherent methods
//! still live in mod.rs since they're impls on the QuerySet inherent
//! surface; they consume `ExplainOptions` / `ExplainFormat` from here.

use super::{run_on, ExecError, RowRef, SqlExecutor};
use crate::core::SelectQuery;
use crate::sql::Pool;

/// Knobs for [`QuerySet::explain_on`](crate::query::QuerySet). Defaults render plain
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

impl ExplainOptions {
    /// Render the parenthesized option list (e.g. `(ANALYZE,
    /// BUFFERS, FORMAT JSON)`) for PostgreSQL. Empty when every option
    /// is at its default.
    fn to_clause(&self) -> String {
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
    Ok(explain_lines_on(pool, query, &options).await?.join("\n"))
}

/// The plan of `query` on `exec`, one entry per plan row ([`explain_pool`]
/// joins them). SQLite's JSON plan is one entry.
pub(super) async fn explain_lines_on<'c, E: SqlExecutor<'c>>(
    exec: E,
    query: &SelectQuery,
    options: &ExplainOptions,
) -> Result<Vec<String>, ExecError> {
    let d = exec.dialect();
    let stmt = d.compile_select(query)?;
    let json = options.format == ExplainFormat::Json;
    let prefix = match d.name() {
        // `EXPLAIN ANALYZE` (8.0.18+) runs the query and wins over
        // `format`; `FORMAT=JSON|TREE` (8.0.16+) is plan-only.
        "mysql" if options.analyze => "EXPLAIN ANALYZE ".to_owned(),
        "mysql" if json => "EXPLAIN FORMAT=JSON ".to_owned(),
        "mysql" => "EXPLAIN FORMAT=TREE ".to_owned(),
        // Plan-only, so analyze/buffers/verbose no-op. Columns: id,
        // parent, notused, detail.
        "sqlite" => "EXPLAIN QUERY PLAN ".to_owned(),
        _ => match options.to_clause() {
            c if c.is_empty() => "EXPLAIN ".to_owned(),
            c => format!("EXPLAIN {c} "),
        },
    };
    let sql = format!("{prefix}{}", stmt.sql);
    let lines = run_on(exec, &sql, stmt.params, |row| match row {
        #[cfg(feature = "postgres")]
        RowRef::Postgres(r) => first_cell(r, json),
        // MySQL's ANALYZE output is text whatever the format.
        #[cfg(feature = "mysql")]
        RowRef::Mysql(r) => first_cell(r, json && !options.analyze),
        #[cfg(feature = "sqlite")]
        RowRef::Sqlite(r) => {
            use sqlx::Row as _;
            let detail: String = r.try_get(3)?;
            if !json {
                return Ok(detail);
            }
            let (id, parent): (i64, i64) = (r.try_get(0)?, r.try_get(1)?);
            Ok(serde_json::json!({"id": id, "parent": parent, "detail": detail}).to_string())
        }
    })
    .await?;
    if json && d.name() == "sqlite" {
        return Ok(vec![format!("[{}]", lines.join(","))]);
    }
    Ok(lines)
}

/// Column 0 of a plan row, as text or as JSON rendered to text.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn first_cell<R>(row: &R, json: bool) -> Result<String, sqlx::Error>
where
    R: sqlx::Row,
    usize: sqlx::ColumnIndex<R>,
    String: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    serde_json::Value: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    if json {
        Ok(row.try_get::<serde_json::Value, _>(0)?.to_string())
    } else {
        row.try_get(0)
    }
}
