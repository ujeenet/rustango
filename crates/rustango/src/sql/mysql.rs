//! The MySQL 8.4+ dialect: backtick-quoted identifiers, `?`
//! placeholders, `BIGINT AUTO_INCREMENT` for an `Auto<T>` PK, `1` and
//! `0` for booleans, and `GET_LOCK` for advisory locks. MySQL has no
//! `RETURNING`.
//!
//! An operator with no MySQL translation gets
//! [`SqlError::OperatorNotSupportedInDialect`] from the writers.

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

/// The singleton [`MySql`] dialect, which
/// [`crate::sql::Pool::dialect`] hands back for a MySQL pool.
#[cfg(feature = "mysql")]
pub static DIALECT: &MySql = &MySql;

impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }

    /// MySQL has no `NULLS FIRST` or `NULLS LAST`, so the writer
    /// sorts on `<col> IS NULL` first instead.
    fn supports_nulls_order(&self) -> bool {
        false
    }

    /// MySQL spells the random function `RAND()`.
    fn random_fn(&self) -> &'static str {
        "RAND"
    }

    /// MySQL rejects an `OFFSET` with no `LIMIT`, so pair it with
    /// the largest `u64` and the limit does nothing.
    fn offset_without_limit_clause(&self) -> Option<&'static str> {
        Some(" LIMIT 18446744073709551615")
    }

    /// MySQL quotes with backticks, and an embedded backtick is
    /// doubled, so any name comes out valid.
    fn quote_ident(&self, name: &str) -> String {
        let escaped = name.replace('`', "``");
        format!("`{escaped}`")
    }

    /// MySQL writes a column comment inline, after the rest of the
    /// column definition. Single quotes are doubled.
    fn write_inline_column_comment(&self, comment: &str) -> Option<String> {
        let escaped = comment.replace('\'', "''");
        Some(format!(" COMMENT '{escaped}'"))
    }

    /// MySQL writes a table comment as a `COMMENT='…'` trailer after
    /// the closing paren. Single quotes are doubled.
    fn write_inline_table_comment(&self, comment: &str) -> Option<String> {
        let escaped = comment.replace('\'', "''");
        Some(format!(" COMMENT='{escaped}'"))
    }

    // `?` placeholders are the trait default.

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "INT AUTO_INCREMENT",
            _ => "BIGINT AUTO_INCREMENT",
        }
    }

    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        // MySQL has no `BIGINT` cast target; it is `SIGNED`.
        format!("CAST({expr} AS SIGNED)")
    }

    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        // MySQL has no `DOUBLE PRECISION`; it is `DOUBLE`.
        format!("CAST({expr} AS DOUBLE)")
    }

    /// MySQL's CAST targets differ from the ANSI names: integers
    /// cast to `SIGNED`, floats to `FLOAT` or `DOUBLE`, booleans to
    /// `UNSIGNED`, strings to `CHAR`, and timestamps to `DATETIME`.
    ///
    /// UUID, JSON and binary have no CAST target, so they return
    /// `None`; cast a string-shaped value to `CHAR` yourself.
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
            // MySQL has no `CAST AS JSON`; use `JSON_EXTRACT`. UUID
            // has no target either, and the array, range and other
            // Postgres-only types have nothing to cast to.
            FieldType::Uuid
            | FieldType::Json
            | FieldType::Array(_)
            | FieldType::Range(_)
            | FieldType::HStore
            | FieldType::Vector(_)
            | FieldType::Geometry(_) => return None,
        })
    }

    /// MySQL's column types differ from the Postgres ones in
    /// several ways:
    /// - no `BOOLEAN`; it is an alias for `TINYINT(1)`
    /// - no `TIMESTAMPTZ`; `DATETIME(6)` holds microseconds but no
    ///   timezone, and sqlx binds a `DateTime<Utc>` into it correctly
    /// - no `JSONB`; `JSON` validates on write and stores binary
    /// - no `UUID`; `CHAR(36)` is the usual form
    /// - `VARCHAR` needs a length, so an unbounded `String` becomes
    ///   `TEXT`
    /// - no `DOUBLE PRECISION`; `DOUBLE`, and `REAL` is an alias for
    ///   `FLOAT`
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
            // A bare `DECIMAL` is `(10, 0)`, which has no fraction.
            // `(38, 10)` is the widest that fits `rust_decimal`.
            FieldType::Decimal => "DECIMAL(38, 10)".into(),
            // `BLOB` caps at 64 KiB, too small here; `LONGBLOB` at
            // 4 GiB.
            FieldType::Binary => "LONGBLOB".into(),
            // Microsecond precision, to match `DATETIME(6)` above.
            FieldType::Time => "TIME(6)".into(),
            // These are all Postgres-only types with no MySQL
            // equivalent, so the column is TEXT and the bind and
            // decode paths reject them.
            FieldType::Array(_) => "TEXT".into(),
            // Ditto for PG range columns (#343).
            FieldType::Range(_) => "TEXT".into(),
            // Ditto for PG hstore columns (#342).
            FieldType::HStore => "TEXT".into(),
            // Ditto for pgvector `vector` columns (#824).
            FieldType::Vector(_) => "TEXT".into(),
            // Ditto for PostGIS `geometry` columns (#443).
            FieldType::Geometry(_) => "TEXT".into(),
        }
    }

    // `utf8mb4_general_ci` is already the default collation in
    // modern MySQL, but setting it per column keeps the comparison
    // case-insensitive even where the table default differs.
    fn ci_text_type(&self, max_length: Option<u32>) -> String {
        match max_length {
            Some(n) => format!("VARCHAR({n}) COLLATE utf8mb4_general_ci"),
            None => "TEXT COLLATE utf8mb4_general_ci".to_owned(),
        }
    }

    /// Translate Postgres-native `DEFAULT` expressions to MySQL
    /// spelling.
    ///
    /// - `now()` becomes `CURRENT_TIMESTAMP(6)`. MySQL wants the
    ///   default's precision to match the column, and a `DateTime`
    ///   column is `DATETIME(6)`; without the `(6)` it rejects the
    ///   default outright.
    /// - `'<lit>'::<type>` becomes `'<lit>'`.
    /// - A JSON, TEXT or BLOB column takes no literal default, only
    ///   MySQL 8.0.13+'s `DEFAULT (<expr>)` form, so those get
    ///   wrapped in parens. `max_length` is what tells an unbounded
    ///   `String`, which is TEXT, from a `VARCHAR(n)`, which keeps
    ///   its literal default.
    /// - Everything else passes through.
    fn translate_default_expr(&self, expr: &str, ty: &str, max_length: Option<u32>) -> String {
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
        let renders_as_lob = ty.eq_ignore_ascii_case("json")
            || ty.eq_ignore_ascii_case("binary")
            || (ty.eq_ignore_ascii_case("string") && max_length.is_none());
        if renders_as_lob {
            // Already an expression; do not wrap it twice.
            if !(out.starts_with('(') && out.ends_with(')')) {
                out = format!("({out})");
            }
        }
        out
    }

    /// MySQL takes `USING BTREE` and `USING HASH`, the latter only
    /// on the MEMORY engine. Any other method is Postgres-only, so
    /// drop the clause and let MySQL build a btree.
    fn index_method_clause(&self, method: &str) -> String {
        match method {
            "hash" => " USING HASH".to_owned(),
            // Anything else, including "btree", needs no clause:
            // MySQL's default is already a btree.
            _ => String::new(),
        }
    }

    /// MySQL has no `CREATE INDEX IF NOT EXISTS`. The migration
    /// ledger already stops a re-run, so nothing is lost.
    fn supports_create_index_if_not_exists(&self) -> bool {
        false
    }

    /// MySQL has no `CREATE INDEX … WHERE <expr>`. The migration
    /// writer drops the WHERE clause and warns, so the index is
    /// still created, just without the filter. If you need a real
    /// partial unique index, check it in the application or add a
    /// CHECK constraint over a generated column.
    fn supports_partial_index(&self) -> bool {
        false
    }

    /// MySQL spells this `DROP CHECK` and takes no `IF EXISTS`,
    /// which is a parse error on any drop-constraint form.
    ///
    /// **So this is not idempotent.** Dropping a constraint that is
    /// already gone is an error, where the Postgres form does
    /// nothing.
    fn drop_check_constraint_sql(&self, table: &str, name: &str) -> Option<String> {
        Some(format!(
            "ALTER TABLE {} DROP CHECK {}",
            self.quote_ident(table),
            self.quote_ident(name)
        ))
    }

    /// MySQL spells this `DROP FOREIGN KEY` and takes no
    /// `IF EXISTS`, so it is not idempotent either.
    fn drop_foreign_key_sql(&self, table: &str, name: &str) -> Option<String> {
        Some(format!(
            "ALTER TABLE {} DROP FOREIGN KEY {}",
            self.quote_ident(table),
            self.quote_ident(name)
        ))
    }

    /// MySQL has no `ON CONFLICT`, so this writes
    /// `ON DUPLICATE KEY UPDATE <col> = <col>`: a no-op assignment
    /// that satisfies its need for at least one. Any of the conflict
    /// columns will do, so it uses the first.
    fn insert_on_conflict_skip(&self, conflict_cols: &[&str]) -> String {
        if conflict_cols.is_empty() {
            return String::new();
        }
        let pivot = conflict_cols[0];
        format!("ON DUPLICATE KEY UPDATE {pivot} = {pivot}")
    }

    /// MySQL's `BOOLEAN` is an alias for `TINYINT(1)`, so emit `1`
    /// and `0` to match how the value is stored.
    fn bool_literal(&self, b: bool) -> &'static str {
        if b {
            "1"
        } else {
            "0"
        }
    }

    // MySQL handles every operator in the IR, through the `write_*`
    // methods below, so the `true` default is right.

    fn write_ilike(&self, sql: &mut String, qualified_col: &str, placeholder: &str, negated: bool) {
        // MySQL has no `ILIKE`. A `_ci` collation would already make
        // `LIKE` case-insensitive, but lowercasing both sides makes
        // it so whatever the column's collation.
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

    /// MySQL's `REGEXP` is case-sensitive and has no insensitive
    /// form; the column's collation decides. As with ILIKE, the
    /// insensitive variants lowercase both sides instead.
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
            // Lowercase both sides in the SQL, so the comparison
            // ignores case whatever the column's collation.
            sql.push_str("LOWER(");
            sql.push_str(qualified_col);
            sql.push(')');
            sql.push_str(kw);
            sql.push_str("LOWER(");
            sql.push_str(placeholder);
            sql.push(')');
        }
    }

    /// MySQL has no trigram operators; its `FULLTEXT` similarity is
    /// a different shape. Reject the query here rather than let the
    /// driver fail on it later.
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

    /// MySQL's full-text search is `MATCH(col) AGAINST(?)`, which
    /// means something different from the Postgres shape, so this is
    /// not translated. Write a raw predicate instead.
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

    /// MySQL has no array type, so reject every array operator.
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

    /// MySQL has no range type, so reject every range operator.
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
        // MySQL's `<=>` is null-safe equality, and
        // `IS DISTINCT FROM` is its negation, so wrap it in `NOT`.
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
        // `JSON_CONTAINS(target, candidate)` is true when every
        // value in the candidate is in the target, the same as
        // Postgres' `target @> candidate`.
        sql.push_str("JSON_CONTAINS(");
        sql.push_str(qualified_col);
        sql.push_str(", ");
        sql.push_str(placeholder);
        sql.push(')');
    }

    fn write_json_contained_by(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        // The same call with the arguments the other way round.
        sql.push_str("JSON_CONTAINS(");
        sql.push_str(placeholder);
        sql.push_str(", ");
        sql.push_str(qualified_col);
        sql.push(')');
    }

    fn write_json_has_key(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        // `JSON_CONTAINS_PATH(col, 'one', '$.key')` is MySQL's
        // top-level key check. `CONCAT('$.', ?)` builds the path
        // from the bound value.
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

    /// MySQL's `ON DUPLICATE KEY UPDATE` takes no target column
    /// list; it fires on any unique violation. So a `DoUpdate` with
    /// no target translates cleanly:
    ///
    /// ```sql
    /// INSERT INTO `t` (a, b) VALUES (?, ?)
    /// ON DUPLICATE KEY UPDATE `a` = VALUES(`a`), `b` = VALUES(`b`)
    /// ```
    ///
    /// `DoNothing` becomes a self-assignment such as
    /// `ON DUPLICATE KEY UPDATE id = id`, which skips the duplicate.
    /// `INSERT IGNORE` would do that too, but it also hides every
    /// other error.
    fn write_conflict_clause(
        &self,
        sql: &mut String,
        conflict: &ConflictClause,
    ) -> Result<(), SqlError> {
        match conflict {
            ConflictClause::DoNothing => {
                // `INSERT IGNORE` would hide every error, including
                // FK violations. A no-op self-update skips only the
                // duplicate.
                sql.push_str(" ON DUPLICATE KEY UPDATE id = id");
            }
            ConflictClause::DoUpdate {
                target,
                update_columns,
            } => {
                // MySQL has no conflict target; the clause matches
                // every unique index. A target is accepted and
                // ignored, so one call site works on every backend.
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

    // MySQL has no transaction-scoped advisory lock.

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
        // Needs MySQL 8.0.19+, for the `VALUES ROW(…)` table
        // constructor. On anything older, update row by row.
        let mut b = Sql::new(self);
        write_mysql_bulk_update(&mut b, query)?;
        Ok(b.finish())
    }
}

/// Write a backtick-quoted identifier in place, for the conflict
/// clause, which writes straight into a `String` rather than through
/// the [`Sql`] builder.
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

/// MySQL's bulk UPDATE: `UPDATE t INNER JOIN (VALUES ROW(…), …) AS
/// d(pk, c1, …) ON t.pk = d.pk SET t.c1 = d.c1`. It lives here
/// rather than in `writers` because the syntax is MySQL's own.
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
    fn default_on_lob_columns_uses_expression_form() {
        // MySQL rejects a literal `DEFAULT` on JSON, TEXT, BLOB and
        // GEOMETRY columns, but accepts the `DEFAULT (<expr>)` form,
        // so those types must be wrapped. An unbounded `String` is
        // one of them, since it renders as TEXT.
        assert_eq!(
            MySql.translate_default_expr("'{}'", "string", None),
            "('{}')"
        );
        assert_eq!(MySql.translate_default_expr("'{}'", "json", None), "('{}')");
        assert_eq!(MySql.translate_default_expr("''", "binary", None), "('')");
        // A bounded String is a VARCHAR(n), which takes a literal
        // default, so it must not be wrapped.
        assert_eq!(
            MySql.translate_default_expr("''", "string", Some(500)),
            "''"
        );
        // Non-LOB scalars are never wrapped.
        assert_eq!(MySql.translate_default_expr("0", "i64", None), "0");
        // now() still becomes the (6)-precision form.
        assert_eq!(
            MySql.translate_default_expr("now()", "datetime", None),
            "CURRENT_TIMESTAMP(6)"
        );
    }

    #[test]
    fn supports_op_accepts_every_operator_after_batch4() {
        // ILIKE, IS DISTINCT FROM and the JSONB operators all
        // translate, so `supports_op` is true for every one.
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
        // The conflict target is ignored: MySQL matches on every
        // unique index anyway.
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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

    /// Copy `model` with `pk_col` marked as the primary key, which
    /// the `bulk_update` test needs.
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

    // Hand-built IR, to confirm the writers and this dialect
    // produce backticks and `?` placeholders, with no `RETURNING`
    // and no NULL casts.

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
            subquery_joins: vec![],
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
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
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
        // MySQL has no RETURNING, so the writer errors rather than
        // emit SQL the parser would reject.
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
                case_insensitive: false,
                fk_on_delete: None,
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
            exclusion_constraints: &[],
            default_permissions: &[],
            composite_relations: &[],
            generic_relations: &[],
            // Every introspected schema is tenant-scoped.
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        }))
    }
}
