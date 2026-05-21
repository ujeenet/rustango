//! `MySQL` 8.4+ dialect — backtick-quoted identifiers, `?` placeholders,
//! `BIGINT AUTO_INCREMENT` for `Auto<T>` PKs, `1`/`0` boolean literals,
//! `GET_LOCK` / `RELEASE_LOCK` for advisory locking.
//!
//! ## v0.23.0 batch status
//!
//! - **batch2** — identity primitives (quoting / placeholders /
//!   serial type / boolean literals / `GET_LOCK`).
//! - **batch3** (this batch) — hooks the IR-to-SQL writers in
//!   [`super::writers`] up to `MySql`. SELECT / COUNT / AGGREGATE /
//!   INSERT (no `RETURNING` — `MySQL` doesn't support it) / UPDATE /
//!   DELETE all work. `INSERT … ON DUPLICATE KEY UPDATE` translates
//!   from a `ConflictClause` shape `MySQL` can express.
//! - **batch4** (planned) — translate `ILIKE` (→ `LOWER(col) LIKE
//!   LOWER(?)`), `IS DISTINCT FROM` (→ `NOT (a <=> b)`), JSON
//!   operators (→ `JSON_CONTAINS` / `JSON_CONTAINS_PATH`), and
//!   `bulk_update` (→ `JOIN`-with-VALUES or `CASE WHEN`).
//!
//! Operators that don't have a one-shot `MySQL` translation today
//! (`ILIKE`, `IS DISTINCT FROM`, JSONB `?` / `?|` / `?&` / `@>` /
//! `<@`) surface a clear
//! [`SqlError::OperatorNotSupportedInDialect`] from the writers when
//! a query tries to use them.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldType, InsertQuery, SelectQuery, UpdateQuery,
};

use super::writers::{
    write_aggregate, write_bulk_insert, write_count, write_delete, write_insert, write_select,
    write_update, Sql,
};
use super::{CompiledStatement, Dialect, SqlError};

/// The `MySQL` 8.4+ dialect. Stateless; construct with `MySql`.
#[derive(Debug, Default, Clone, Copy)]
pub struct MySql;

/// `'static` reference to the singleton [`MySql`] dialect, symmetric
/// with [`super::postgres::DIALECT`]. Used by [`crate::sql::Pool::dialect`]
/// to hand back a `&'static dyn Dialect` regardless of pool variant.
pub static DIALECT: &MySql = &MySql;

impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }

    /// MySQL has no native `NULLS FIRST` / `NULLS LAST` keyword.
    /// Returning `false` makes `write_order_limit_offset` emulate
    /// via an `<col> IS NULL` pre-sort term — issue #76.
    fn supports_nulls_order(&self) -> bool {
        false
    }

    /// MySQL spells the random function as `RAND()`, not `RANDOM()`.
    /// Issue #77.
    fn random_fn(&self) -> &'static str {
        "RAND"
    }

    /// `MySQL` quotes identifiers with backticks, not double quotes.
    /// Embedded backticks are doubled (the `MySQL` parser's escape rule)
    /// so the output is always a valid quoted identifier even for
    /// pathological column names.
    fn quote_ident(&self, name: &str) -> String {
        let escaped = name.replace('`', "``");
        format!("`{escaped}`")
    }

    /// MySQL spells column comments inline in the CREATE TABLE column
    /// definition (`<col> <type> [NULL/NOT NULL] [DEFAULT …] COMMENT
    /// '<escaped>'`). Single quotes are doubled per the MySQL parser's
    /// escape rule.
    fn write_inline_column_comment(&self, comment: &str) -> Option<String> {
        let escaped = comment.replace('\'', "''");
        Some(format!(" COMMENT '{escaped}'"))
    }

    // `?`-style placeholders are the trait default — no override needed.

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "INT AUTO_INCREMENT",
            _ => "BIGINT AUTO_INCREMENT",
        }
    }

    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        // MySQL: `CAST(<expr> AS BIGINT)` doesn't work — the integer
        // target is `SIGNED` (or `UNSIGNED`).
        format!("CAST({expr} AS SIGNED)")
    }

    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        // MySQL doesn't have `DOUBLE PRECISION`; `DOUBLE` is the right
        // target for AVG widening.
        format!("CAST({expr} AS DOUBLE)")
    }

    /// MySQL CAST type tokens. Differs from the trait default in that:
    /// - Integer types use `SIGNED` (8.x supports `BIGINT`/`INT` since
    ///   8.0.17 but `SIGNED` works on 5.7 + 8.x uniformly — pick the
    ///   widest-compatible token).
    /// - Floats: `FLOAT`/`DOUBLE` (no `DOUBLE PRECISION` token).
    /// - Bool: `UNSIGNED` (TINYINT(1) isn't a valid CAST target).
    /// - String: `CHAR` (TEXT isn't a valid CAST target on MySQL).
    /// - DateTime: `DATETIME` (no `TIMESTAMPTZ`).
    /// - UUID / JSON / Binary: not supported as CAST targets — return
    ///   `None`. Caller can serialize to a `CHAR` if string-shaped.
    fn cast_type(&self, ty: FieldType) -> Option<&'static str> {
        Some(match ty {
            FieldType::I16 | FieldType::I32 | FieldType::I64 => "SIGNED",
            FieldType::F32 => "FLOAT",
            FieldType::F64 => "DOUBLE",
            FieldType::Bool => "UNSIGNED",
            FieldType::String => "CHAR",
            FieldType::DateTime => "DATETIME",
            FieldType::Date => "DATE",
            FieldType::Time => "TIME",
            FieldType::Decimal => "DECIMAL(38, 10)",
            FieldType::Binary => "BINARY",
            // MySQL has no `CAST AS JSON` form — `JSON_EXTRACT` /
            // string parsing are the substitutes. UUID has no CAST
            // target either (it's CHAR(36) in DDL but the user would
            // cast to CHAR in expression form).
            FieldType::Uuid | FieldType::Json => return None,
        })
    }

    /// `MySQL` column types — major divergences from the ANSI / Postgres
    /// shape:
    /// - no native `BOOLEAN`; `BOOL`/`BOOLEAN` are aliases for `TINYINT(1)`
    /// - no `TIMESTAMPTZ`; `DATETIME(6)` is microsecond-precision and
    ///   timezone-naive (the framework's chrono::DateTime<Utc> binds
    ///   into it correctly via sqlx-mysql)
    /// - no `JSONB`; `JSON` is the JSON type (validates on write,
    ///   stores as binary internally)
    /// - no `UUID`; `CHAR(36)` is the canonical string form
    /// - `TEXT` exists but unbounded `VARCHAR` doesn't — `VARCHAR`
    ///   without a length is rejected by the parser, so an
    ///   unbounded `String` field becomes `TEXT`
    /// - `DOUBLE PRECISION` is `DOUBLE`; `REAL` is an alias for
    ///   `FLOAT` in MySQL (different from Postgres' 4-byte REAL —
    ///   close enough for the framework's f32/f64 mapping)
    fn column_type(&self, ty: FieldType, max_length: Option<u32>) -> String {
        match ty {
            FieldType::I16 => "SMALLINT".into(),
            FieldType::I32 => "INT".into(),
            FieldType::I64 => "BIGINT".into(),
            FieldType::F32 => "FLOAT".into(),
            FieldType::F64 => "DOUBLE".into(),
            FieldType::Bool => "TINYINT(1)".into(),
            FieldType::String => match max_length {
                Some(n) => format!("VARCHAR({n})"),
                None => "TEXT".into(),
            },
            FieldType::DateTime => "DATETIME(6)".into(),
            FieldType::Date => "DATE".into(),
            FieldType::Uuid => "CHAR(36)".into(),
            FieldType::Json => "JSON".into(),
            // MySQL `DECIMAL` defaults to `(10, 0)` — useless for app
            // models. `(38, 10)` is the widest portable precision that
            // matches `rust_decimal::Decimal`'s capacity. Per-column
            // overrides land when we expose `precision`/`scale` attrs.
            FieldType::Decimal => "DECIMAL(38, 10)".into(),
            // `LONGBLOB` lifts the cap to 4 GiB; `BLOB` is 64 KiB,
            // which is too small for a generic `BinaryField`.
            FieldType::Binary => "LONGBLOB".into(),
            // `TIME(6)` for microsecond precision — matches the
            // `DATETIME(6)` choice elsewhere in this writer.
            FieldType::Time => "TIME(6)".into(),
        }
    }

    /// Translate Postgres-native `DEFAULT` expressions to MySQL
    /// spelling.
    ///
    /// - `now()` / `CURRENT_TIMESTAMP` → `CURRENT_TIMESTAMP(6)`. MySQL
    ///   requires the fractional-second precision of the `DEFAULT` to
    ///   match the column type; our `DateTime` columns render as
    ///   `DATETIME(6)` (see [`Self::column_type`]) so the default has
    ///   to carry `(6)` too, otherwise MySQL fails with
    ///   `1067 (42000): Invalid default value`.
    /// - `'<lit>'::<type>` → `'<lit>'` (strip Postgres cast).
    /// - JSON columns (`ty == "json"`): wrap the result in parens —
    ///   `DEFAULT '{}'` is rejected by MySQL on JSON/TEXT/BLOB
    ///   (`1101 (42000)`), but the MySQL-8.0.13+ expression-default
    ///   form `DEFAULT (<expr>)` is accepted.
    /// - Everything else passes through.
    fn translate_default_expr(&self, expr: &str, ty: &str) -> String {
        let mut out = expr.trim().to_owned();
        match out.as_str() {
            "now()" | "NOW()" | "current_timestamp" | "CURRENT_TIMESTAMP" => {
                out = "CURRENT_TIMESTAMP(6)".to_owned();
            }
            _ => {
                if let Some(idx) = out.rfind("::") {
                    let suffix = &out[idx + 2..];
                    if !suffix.is_empty()
                        && suffix
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        out.truncate(idx);
                    }
                }
            }
        }
        if ty.eq_ignore_ascii_case("json") {
            // Skip wrapping if the caller already produced a
            // parenthesized expression (defensive — current renderers
            // never do this).
            if !(out.starts_with('(') && out.ends_with(')')) {
                out = format!("({out})");
            }
        }
        out
    }

    /// MySQL does not accept `CREATE INDEX IF NOT EXISTS` (as of
    /// 8.x — parse error). The ledger already serializes migration
    /// application so the idempotency guard is unnecessary in
    /// practice.
    /// MySQL accepts `USING BTREE` and `USING HASH` (the latter is
    /// MEMORY-engine-only and silently downgraded to BTREE on
    /// InnoDB). Any other method is a PG-ism — drop the clause and
    /// let MySQL build a regular btree. Issue #34.
    fn index_method_clause(&self, method: &str) -> String {
        match method {
            "hash" => " USING HASH".to_owned(),
            // Everything else (including the explicit "btree") emits
            // no clause — MySQL's default is already btree.
            _ => String::new(),
        }
    }

    fn supports_create_index_if_not_exists(&self) -> bool {
        false
    }

    /// MySQL has no `CREATE INDEX ... WHERE <expr>` syntax. Issue #265
    /// / T1.3. The migration writer drops the WHERE clause + surfaces
    /// a warning so a partial-unique declaration still creates the
    /// underlying UNIQUE index (just without the partial filter).
    /// Document the limitation; users requiring strict partial
    /// uniqueness on MySQL should add an application-level check or
    /// a CHECK constraint over a generated column.
    fn supports_partial_index(&self) -> bool {
        false
    }

    /// MySQL has no `ON CONFLICT`. The semantic equivalent is
    /// `ON DUPLICATE KEY UPDATE <col> = <col>` — a no-op write
    /// against an existing row that satisfies MySQL's requirement
    /// that the clause name at least one column assignment. Picks
    /// the first conflict column so the SQL parses against any
    /// unique-index shape the caller passes in.
    fn insert_on_conflict_skip(&self, conflict_cols: &[&str]) -> String {
        if conflict_cols.is_empty() {
            return String::new();
        }
        let pivot = conflict_cols[0];
        format!("ON DUPLICATE KEY UPDATE {pivot} = {pivot}")
    }

    /// `MySQL` has no native `BOOLEAN` (the `BOOL` keyword is just an
    /// alias for `TINYINT(1)`). Emit `1`/`0` so `DEFAULT` clauses and
    /// inline comparisons match the storage shape.
    fn bool_literal(&self, b: bool) -> &'static str {
        if b {
            "1"
        } else {
            "0"
        }
    }

    // `MySQL` supports every operator in the IR — batch4 ships
    // translations for `ILIKE`, `IS DISTINCT FROM`, and the JSONB
    // operators via the per-op `write_*` methods below. Trait default
    // `true` is correct, no override needed.

    fn write_ilike(&self, sql: &mut String, qualified_col: &str, placeholder: &str, negated: bool) {
        // `MySQL` has no native `ILIKE`; collation may make `LIKE`
        // case-insensitive on `_ci` columns, but to guarantee semantics
        // independent of column collation, lowercase both sides.
        sql.push_str("LOWER(");
        sql.push_str(qualified_col);
        sql.push_str(if negated {
            ") NOT LIKE LOWER("
        } else {
            ") LIKE LOWER("
        });
        sql.push_str(placeholder);
        sql.push(')');
    }

    /// MySQL has `REGEXP` for case-sensitive matching but no native
    /// case-insensitive operator — column collation drives it.
    /// To guarantee case-insensitivity independent of collation
    /// (mirror of the ILIKE approach), lowercase both sides for the
    /// `IRegex` / `NotIRegex` variants. Issue #26.
    fn write_regex(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        case_sensitive: bool,
        negated: bool,
    ) {
        let kw = if negated { " NOT REGEXP " } else { " REGEXP " };
        if case_sensitive {
            sql.push_str(qualified_col);
            sql.push_str(kw);
            sql.push_str(placeholder);
        } else {
            // LOWER(<col>) REGEXP LOWER(<p>) — the pattern is bound
            // as a single param, so wrapping in LOWER(...) at SQL
            // level forces the comparison to be case-insensitive
            // regardless of the column's collation.
            sql.push_str("LOWER(");
            sql.push_str(qualified_col);
            sql.push(')');
            sql.push_str(kw);
            sql.push_str("LOWER(");
            sql.push_str(placeholder);
            sql.push(')');
        }
    }

    /// `pg_trgm` trigram operators are Postgres-only. MySQL has no
    /// equivalent (text similarity via `FULLTEXT` is a different
    /// shape). Reject at compile time so the user retargets the
    /// query rather than discovering the gap at run time. Issue #29.
    fn write_trigram_similar(
        &self,
        _sql: &mut String,
        _qualified_col: &str,
        _placeholder: &str,
        word: bool,
    ) -> Result<(), super::SqlError> {
        Err(super::SqlError::OpNotSupportedInDialect {
            op: if word {
                "trigram_word_similar (%>) — pg_trgm is Postgres-only"
            } else {
                "trigram_similar (%) — pg_trgm is Postgres-only"
            },
            dialect: "mysql",
        })
    }

    /// Postgres-shape FTS (`to_tsvector @@ plainto_tsquery`) doesn't
    /// translate to MySQL. MySQL has its own `MATCH(col) AGAINST(?)`
    /// shape with different semantics; the rustango FTS DSL doesn't
    /// auto-port the lookup. Reject at compile time so the user
    /// either retargets the backend or builds a MySQL-specific raw
    /// `WhereExpr::Raw` predicate. Issue #28.
    fn write_search(
        &self,
        _sql: &mut String,
        _qualified_col: &str,
        _placeholder: &str,
    ) -> Result<(), super::SqlError> {
        Err(super::SqlError::OpNotSupportedInDialect {
            op: "search (__search) — full-text search shape is Postgres-only; \
                 use MySQL `MATCH … AGAINST` via a raw predicate",
            dialect: "mysql",
        })
    }

    /// MySQL has no native array type. Reject every PG array op at
    /// compile time. Issue #30.
    fn write_array_op(
        &self,
        _sql: &mut String,
        _qualified_col: &str,
        _placeholder: &str,
        _op: &'static str,
    ) -> Result<(), super::SqlError> {
        Err(super::SqlError::OpNotSupportedInDialect {
            op: "array operators (@>, <@, &&) — PG ArrayField is Postgres-only; \
                 use JSON columns + JSON-shape operators on MySQL",
            dialect: "mysql",
        })
    }

    /// MySQL has no native range type. Reject every PG range op at
    /// compile time. Issue #31.
    fn write_range_op(
        &self,
        _sql: &mut String,
        _qualified_col: &str,
        _placeholder: &str,
        _op: &'static str,
    ) -> Result<(), super::SqlError> {
        Err(super::SqlError::OpNotSupportedInDialect {
            op: "range operators (@>, <@, &&, <<, >>, -|-) — PG RangeField is Postgres-only; \
                 store lo/hi as separate columns on MySQL",
            dialect: "mysql",
        })
    }

    fn write_null_safe_eq(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        distinct: bool,
    ) {
        // MySQL `<=>` is null-safe equality (`NULL <=> NULL` → 1).
        // `IS DISTINCT FROM` is the *negation* of null-safe equality,
        // so wrap with `NOT (…)` when `distinct = true`.
        if distinct {
            sql.push_str("NOT (");
        }
        sql.push_str(qualified_col);
        sql.push_str(" <=> ");
        sql.push_str(placeholder);
        if distinct {
            sql.push(')');
        }
    }

    fn write_json_contains(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        // MySQL: `JSON_CONTAINS(target, candidate)` returns 1 if every
        // value in `candidate` exists in `target`. Order matches the
        // Postgres `target @> candidate` semantics.
        sql.push_str("JSON_CONTAINS(");
        sql.push_str(qualified_col);
        sql.push_str(", ");
        sql.push_str(placeholder);
        sql.push(')');
    }

    fn write_json_contained_by(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        // PG `target <@ candidate` ↔ `JSON_CONTAINS(candidate, target)`.
        // Argument order is swapped compared to the contains case.
        sql.push_str("JSON_CONTAINS(");
        sql.push_str(placeholder);
        sql.push_str(", ");
        sql.push_str(qualified_col);
        sql.push(')');
    }

    fn write_json_has_key(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        // PG `col ? 'key'` checks top-level key existence on a JSONB
        // value. MySQL has `JSON_CONTAINS_PATH(col, 'one', '$.key')`
        // for the same check; we assemble `'$.<key>'` at runtime via
        // `CONCAT('$.', ?)` so the path is built from the bound value.
        sql.push_str("JSON_CONTAINS_PATH(");
        sql.push_str(qualified_col);
        sql.push_str(", 'one', CONCAT('$.', ");
        sql.push_str(placeholder);
        sql.push_str("))");
    }

    fn write_json_has_any_keys(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholders: &[String],
    ) {
        write_my_json_has_keys(sql, qualified_col, placeholders, "one");
    }

    fn write_json_has_all_keys(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholders: &[String],
    ) {
        write_my_json_has_keys(sql, qualified_col, placeholders, "all");
    }

    /// `MySQL`'s `INSERT … ON DUPLICATE KEY UPDATE` doesn't take a
    /// target column list — it triggers on any unique violation —
    /// so a `DoUpdate` with a non-empty `target` cannot be translated
    /// 1:1 (writer surfaces a clear error). `DoUpdate` with empty
    /// `target` translates cleanly:
    ///
    /// ```sql
    /// INSERT INTO `t` (a, b) VALUES (?, ?)
    /// ON DUPLICATE KEY UPDATE `a` = VALUES(`a`), `b` = VALUES(`b`)
    /// ```
    ///
    /// `DoNothing` translates to `INSERT IGNORE`-equivalent
    /// `ON DUPLICATE KEY UPDATE id = id` — the no-op assignment trick
    /// lets the same INSERT path silently skip duplicates without
    /// switching to the `INSERT IGNORE` keyword (which would also
    /// swallow other recoverable errors). Caller picks the column to
    /// reuse — typically the PK.
    fn write_conflict_clause(
        &self,
        sql: &mut String,
        conflict: &ConflictClause,
    ) -> Result<(), SqlError> {
        match conflict {
            ConflictClause::DoNothing => {
                // `INSERT IGNORE` would skip *all* errors (FK violations
                // included), which we don't want; the no-op self-update
                // trick below is the standard way to silently skip
                // duplicates without losing other error visibility.
                sql.push_str(" ON DUPLICATE KEY UPDATE id = id");
            }
            ConflictClause::DoUpdate {
                target,
                update_columns,
            } => {
                // MySQL has no conflict-target concept — `ON DUPLICATE
                // KEY UPDATE` matches every UNIQUE index automatically.
                // We accept a non-empty `target` for tri-dialect call-
                // site uniformity (PG / SQLite need it) and silently
                // ignore the value here. Issue #267 — pre-T1.5 this
                // path errored; the bulk_upsert API now relies on the
                // silent-ignore.
                let _ = target;
                if update_columns.is_empty() {
                    return Err(SqlError::EmptyUpdateSet);
                }
                sql.push_str(" ON DUPLICATE KEY UPDATE ");
                let mut first = true;
                for col in update_columns {
                    if !first {
                        sql.push_str(", ");
                    }
                    first = false;
                    write_my_ident(sql, col);
                    sql.push_str(" = VALUES(");
                    write_my_ident(sql, col);
                    sql.push(')');
                }
            }
        }
        Ok(())
    }

    // ---- advisory locks ----

    fn acquire_session_lock_sql(&self) -> Option<String> {
        Some(format!("SELECT GET_LOCK({}, -1)", self.placeholder(1)))
    }

    fn release_session_lock_sql(&self) -> Option<String> {
        Some(format!("SELECT RELEASE_LOCK({})", self.placeholder(1)))
    }

    // `MySQL` has no transaction-scoped advisory lock — `None` is the
    // honest answer; the migration runner handles it in batch5.

    // ---- compilation ----

    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_select(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_count(&self, query: &CountQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_count(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_aggregate(&self, query: &AggregateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_aggregate(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::with_capacity(self, query.values.len());
        write_insert(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_bulk_insert(&self, query: &BulkInsertQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::with_capacity(self, query.columns.len() * query.rows.len());
        write_bulk_insert(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_update(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_delete(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_bulk_update(&self, query: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError> {
        // MySQL 8.0.19+ supports `VALUES ROW(…), ROW(…)` table
        // constructors that can be JOINed in `UPDATE … INNER JOIN
        // (VALUES …) AS d(pk, c1, …) ON t.pk = d.pk SET t.c1 = d.c1`.
        // Apps on older MySQL fall back to per-row `compile_update`.
        let mut b = Sql::new(self);
        write_mysql_bulk_update(&mut b, query)?;
        Ok(b.finish())
    }
}

/// Backtick-quoted identifier writer used by
/// [`MySql::write_conflict_clause`] — the conflict clause writes
/// directly into a `&mut String`, so we need a small helper that
/// doesn't go through the [`Sql`] builder.
fn write_my_ident(sql: &mut String, name: &str) {
    sql.push('`');
    for ch in name.chars() {
        if ch == '`' {
            sql.push_str("``");
        } else {
            sql.push(ch);
        }
    }
    sql.push('`');
}

/// Shared body of [`MySql::write_json_has_any_keys`] /
/// [`MySql::write_json_has_all_keys`]. `mode` is `"one"` for "any key
/// matches" or `"all"` for "every key matches".
fn write_my_json_has_keys(
    sql: &mut String,
    qualified_col: &str,
    placeholders: &[String],
    mode: &'static str,
) {
    sql.push_str("JSON_CONTAINS_PATH(");
    sql.push_str(qualified_col);
    sql.push_str(", '");
    sql.push_str(mode);
    sql.push('\'');
    for p in placeholders {
        sql.push_str(", CONCAT('$.', ");
        sql.push_str(p);
        sql.push(')');
    }
    sql.push(')');
}

/// MySQL bulk UPDATE: rewrite Postgres' `UPDATE t SET … FROM (VALUES …)`
/// to `UPDATE t INNER JOIN (VALUES ROW(…), ROW(…)) AS d(pk, c1, …)
/// ON t.pk = d.pk SET t.c1 = d.c1, …` (the multi-row VALUES + JOIN
/// shape MySQL 8.0.19+ supports). The dialect dispatches to this from
/// `compile_bulk_update` — kept here rather than in `writers` because
/// the syntax is MySQL-specific.
fn write_mysql_bulk_update(
    b: &mut crate::sql::writers::Sql<'_>,
    query: &crate::core::BulkUpdateQuery,
) -> Result<(), SqlError> {
    use std::fmt::Write as _;

    if query.rows.is_empty() {
        return Err(SqlError::EmptyBulkInsert);
    }
    if query.update_columns.is_empty() {
        return Err(SqlError::EmptyUpdateSet);
    }
    let pk_field = query
        .model
        .primary_key()
        .ok_or(SqlError::MissingPrimaryKey)?;

    b.sql.push_str("UPDATE ");
    b.write_ident(query.model.table);
    b.sql.push_str(" INNER JOIN (VALUES ");
    let mut first_row = true;
    for row in &query.rows {
        if !first_row {
            b.sql.push_str(", ");
        }
        first_row = false;
        b.sql.push_str("ROW(");
        for (i, val) in row.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.params.push(val.clone());
            let _ = write!(b.sql, "{}", b.d.placeholder(b.params.len()));
        }
        b.sql.push(')');
    }
    b.sql.push_str(") AS __data(");
    b.write_ident(pk_field.column);
    for col in &query.update_columns {
        b.sql.push_str(", ");
        b.write_ident(col);
    }
    b.sql.push_str(") ON ");
    b.write_ident(query.model.table);
    b.sql.push('.');
    b.write_ident(pk_field.column);
    b.sql.push_str(" = __data.");
    b.write_ident(pk_field.column);
    b.sql.push_str(" SET ");
    let mut first = true;
    for col in &query.update_columns {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        b.write_ident(query.model.table);
        b.sql.push('.');
        b.write_ident(col);
        b.sql.push_str(" = __data.");
        b.write_ident(col);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::FieldType;

    #[test]
    fn name_is_mysql() {
        assert_eq!(MySql.name(), "mysql");
    }

    #[test]
    fn quote_ident_uses_backticks() {
        assert_eq!(MySql.quote_ident("col"), "`col`");
        assert_eq!(MySql.quote_ident("schema.table"), "`schema.table`");
    }

    #[test]
    fn quote_ident_escapes_embedded_backticks() {
        assert_eq!(MySql.quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn placeholder_is_question_mark() {
        assert_eq!(MySql.placeholder(1), "?");
        assert_eq!(MySql.placeholder(7), "?");
    }

    #[test]
    fn serial_type_uses_auto_increment() {
        assert_eq!(MySql.serial_type(FieldType::I32), "INT AUTO_INCREMENT");
        assert_eq!(MySql.serial_type(FieldType::I64), "BIGINT AUTO_INCREMENT");
    }

    #[test]
    fn bool_literal_uses_one_zero() {
        assert_eq!(MySql.bool_literal(true), "1");
        assert_eq!(MySql.bool_literal(false), "0");
    }

    #[test]
    fn null_cast_returns_none() {
        // MySQL doesn't need NULL casts — sqlx binds the right type.
        assert!(MySql.null_cast(FieldType::I32).is_none());
        assert!(MySql.null_cast(FieldType::String).is_none());
    }

    #[test]
    fn does_not_support_returning() {
        assert!(!MySql.supports_returning());
    }

    #[test]
    fn does_not_support_concurrent_index() {
        assert!(!MySql.supports_concurrent_index());
    }

    #[test]
    fn supports_op_accepts_every_operator_after_batch4() {
        // batch4 ships translations for ILIKE, IS DISTINCT FROM, and
        // the JSONB operators; supports_op now returns true for all.
        use crate::core::Op;
        for op in [
            Op::Eq,
            Op::Ne,
            Op::Lt,
            Op::Lte,
            Op::Gt,
            Op::Gte,
            Op::In,
            Op::NotIn,
            Op::Like,
            Op::NotLike,
            Op::ILike,
            Op::NotILike,
            Op::Between,
            Op::IsNull,
            Op::IsDistinctFrom,
            Op::IsNotDistinctFrom,
            Op::JsonContains,
            Op::JsonContainedBy,
            Op::JsonHasKey,
            Op::JsonHasAnyKey,
            Op::JsonHasAllKeys,
        ] {
            assert!(MySql.supports_op(op), "expected {op:?} to be supported");
        }
    }

    #[test]
    fn session_lock_uses_get_lock() {
        let acq = MySql.acquire_session_lock_sql().unwrap();
        assert!(acq.contains("GET_LOCK"));
        assert!(acq.contains("?"));
        let rel = MySql.release_session_lock_sql().unwrap();
        assert!(rel.contains("RELEASE_LOCK"));
    }

    #[test]
    fn xact_lock_is_none() {
        assert!(MySql.acquire_xact_lock_sql().is_none());
    }

    #[test]
    fn conflict_do_nothing_emits_no_op_update() {
        let mut sql = String::new();
        MySql
            .write_conflict_clause(&mut sql, &ConflictClause::DoNothing)
            .unwrap();
        assert_eq!(sql, " ON DUPLICATE KEY UPDATE id = id");
    }

    #[test]
    fn conflict_do_update_with_empty_target_translates() {
        let mut sql = String::new();
        MySql
            .write_conflict_clause(
                &mut sql,
                &ConflictClause::DoUpdate {
                    target: vec![],
                    update_columns: vec!["a", "b"],
                },
            )
            .unwrap();
        assert_eq!(
            sql,
            " ON DUPLICATE KEY UPDATE `a` = VALUES(`a`), `b` = VALUES(`b`)"
        );
    }

    #[test]
    fn conflict_do_update_with_target_silently_ignores_target() {
        // Issue #267 / T1.5 — MySQL silently ignores the conflict
        // target (it matches on every UNIQUE index automatically).
        // Pre-T1.5 the writer errored; the bulk_upsert API needs
        // the silent-ignore for tri-dialect call-site uniformity.
        let mut sql = String::new();
        MySql
            .write_conflict_clause(
                &mut sql,
                &ConflictClause::DoUpdate {
                    target: vec!["id"],
                    update_columns: vec!["a"],
                },
            )
            .expect("MySQL should accept target and silently ignore it");
        assert_eq!(sql, " ON DUPLICATE KEY UPDATE `a` = VALUES(`a`)");
    }

    #[test]
    fn ilike_translates_to_lower_like_lower() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("users", &[("name", FieldType::String)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "name",
                op: Op::ILike,
                value: SqlValue::String("%Alice%".into()),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert_eq!(
            stmt.sql,
            "SELECT `name` FROM `users` WHERE LOWER(`name`) LIKE LOWER(?)"
        );
        assert_eq!(stmt.params.len(), 1);
    }

    #[test]
    fn not_ilike_translates_to_not_like() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("users", &[("name", FieldType::String)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "name",
                op: Op::NotILike,
                value: SqlValue::String("%bot%".into()),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt.sql.contains("LOWER(`name`) NOT LIKE LOWER(?)"));
    }

    #[test]
    fn is_distinct_from_translates_to_not_null_safe_eq() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("users", &[("email", FieldType::String)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "email",
                op: Op::IsDistinctFrom,
                value: SqlValue::String("a@b".into()),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt.sql.contains("NOT (`email` <=> ?)"));
    }

    #[test]
    fn is_not_distinct_from_translates_to_null_safe_eq() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("users", &[("email", FieldType::String)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "email",
                op: Op::IsNotDistinctFrom,
                value: SqlValue::Null,
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        // No outer NOT; bare null-safe equality.
        assert!(stmt.sql.contains("`email` <=> ?"));
        assert!(!stmt.sql.contains("NOT"));
    }

    #[test]
    fn json_contains_translates_to_json_contains_function() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("posts", &[("meta", FieldType::Json)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "meta",
                op: Op::JsonContains,
                value: SqlValue::Json(serde_json::json!({"k": "v"})),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt.sql.contains("JSON_CONTAINS(`meta`, ?)"));
        assert!(!stmt.sql.contains("@>"));
    }

    #[test]
    fn json_contained_by_swaps_argument_order() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("posts", &[("meta", FieldType::Json)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "meta",
                op: Op::JsonContainedBy,
                value: SqlValue::Json(serde_json::json!({"k": "v"})),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        // Argument order is swapped vs JSON_CONTAINS — value first.
        assert!(stmt.sql.contains("JSON_CONTAINS(?, `meta`)"));
    }

    #[test]
    fn json_has_key_translates_to_contains_path_with_concat() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("posts", &[("meta", FieldType::Json)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "meta",
                op: Op::JsonHasKey,
                value: SqlValue::String("title".into()),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt
            .sql
            .contains("JSON_CONTAINS_PATH(`meta`, 'one', CONCAT('$.', ?))"));
    }

    #[test]
    fn json_has_any_keys_translates_to_contains_path_one() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("posts", &[("meta", FieldType::Json)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "meta",
                op: Op::JsonHasAnyKey,
                value: SqlValue::List(vec![
                    SqlValue::String("title".into()),
                    SqlValue::String("body".into()),
                ]),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt
            .sql
            .contains("JSON_CONTAINS_PATH(`meta`, 'one', CONCAT('$.', ?), CONCAT('$.', ?))"));
        assert_eq!(stmt.params.len(), 2);
    }

    #[test]
    fn json_has_all_keys_uses_all_mode() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("posts", &[("meta", FieldType::Json)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "meta",
                op: Op::JsonHasAllKeys,
                value: SqlValue::List(vec![
                    SqlValue::String("a".into()),
                    SqlValue::String("b".into()),
                ]),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt
            .sql
            .contains("JSON_CONTAINS_PATH(`meta`, 'all', CONCAT('$.', ?), CONCAT('$.', ?))"));
    }

    #[test]
    fn bulk_update_translates_to_inner_join_values_row() {
        use crate::core::{BulkUpdateQuery, SqlValue};
        let model = empty_model_with(
            "users",
            &[("id", FieldType::I64), ("name", FieldType::String)],
        );
        // Mark the id field as the PK so primary_key() resolves.
        let pk_model = with_pk(model, "id");
        let q = BulkUpdateQuery {
            model: pk_model,
            update_columns: vec!["name"],
            rows: vec![
                vec![SqlValue::I64(1), SqlValue::String("Alice".into())],
                vec![SqlValue::I64(2), SqlValue::String("Bob".into())],
            ],
        };
        let stmt = MySql.compile_bulk_update(&q).unwrap();
        // Spot-check the key shape: VALUES ROW(?, ?) plus the JOIN
        // on the PK plus the SET on the qualified target column.
        assert!(stmt.sql.starts_with("UPDATE `users` INNER JOIN (VALUES "));
        assert!(stmt.sql.contains("ROW(?, ?), ROW(?, ?)"));
        assert!(stmt.sql.contains(") AS __data(`id`, `name`)"));
        assert!(stmt.sql.contains("ON `users`.`id` = __data.`id`"));
        assert!(stmt.sql.contains("SET `users`.`name` = __data.`name`"));
        assert_eq!(stmt.params.len(), 4);
    }

    /// Build a model identical to `model` but with `pk_col` flipped to
    /// `primary_key = true`. Used by `bulk_update` test since
    /// `primary_key()` returns `None` otherwise.
    fn with_pk(
        model: &'static crate::core::ModelSchema,
        pk_col: &'static str,
    ) -> &'static crate::core::ModelSchema {
        let new_fields: Vec<crate::core::FieldSchema> = model
            .fields
            .iter()
            .map(|f| {
                let mut f = *f;
                if f.column == pk_col {
                    f.primary_key = true;
                }
                f
            })
            .collect();
        let leaked: &'static [crate::core::FieldSchema] = Box::leak(new_fields.into_boxed_slice());
        Box::leak(Box::new(crate::core::ModelSchema {
            fields: leaked,
            ..*model
        }))
    }

    // -------- writers integration smoke tests --------
    //
    // Construct minimal IR by hand to confirm the writers + MySql
    // dialect glue produces backticks + ? placeholders, with no
    // `RETURNING` and no NULL casts.

    #[test]
    fn select_emits_backticks_and_question_marks() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with(
            "users",
            &[("id", FieldType::I64), ("name", FieldType::String)],
        );
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "name",
                op: Op::Eq,
                value: SqlValue::String("alice".into()),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert_eq!(
            stmt.sql,
            "SELECT `id`, `name` FROM `users` WHERE `name` = ?"
        );
        assert_eq!(stmt.params.len(), 1);
    }

    #[test]
    fn insert_with_returning_errors() {
        // MySQL has no RETURNING — the writer surfaces a clear error
        // instead of emitting Postgres-shape SQL the MySQL parser
        // would reject.
        use crate::core::{InsertQuery, SqlValue};
        let model = empty_model_with(
            "users",
            &[("id", FieldType::I64), ("name", FieldType::String)],
        );
        let q = InsertQuery {
            model,
            columns: vec!["name"],
            values: vec![SqlValue::String("alice".into())],
            returning: vec!["id"],
            on_conflict: None,
        };
        let err = MySql.compile_insert(&q).unwrap_err();
        assert!(matches!(
            err,
            SqlError::OperatorNotSupportedInDialect {
                op: "RETURNING",
                dialect: "mysql"
            }
        ));
    }

    fn empty_model_with(
        table: &'static str,
        fields: &[(&'static str, FieldType)],
    ) -> &'static crate::core::ModelSchema {
        // Build a minimal ModelSchema for tests. Fields are leaked
        // for `'static` lifetime — fine in test code.
        let field_vec: Vec<crate::core::FieldSchema> = fields
            .iter()
            .map(|(col, ty)| crate::core::FieldSchema {
                name: col,
                column: col,
                ty: *ty,
                nullable: false,
                primary_key: false,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                validators: &[],
            })
            .collect();
        let leaked: &'static [crate::core::FieldSchema] = Box::leak(field_vec.into_boxed_slice());
        Box::leak(Box::new(crate::core::ModelSchema {
            name: table,
            table,
            fields: leaked,
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            permissions: false,
            audit_track: None,
            m2m: &[],
            indexes: &[],
            check_constraints: &[],
            composite_relations: &[],
            generic_relations: &[],
            // v0.27.7 — every introspected schema is tenant-scoped
            // (mysql introspection isn't used for registry models).
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
        }))
    }
}
