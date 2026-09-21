//! The SQLite 3.35+ dialect: ANSI double-quoted identifiers, `?`
//! placeholders, `INTEGER PRIMARY KEY AUTOINCREMENT` for an `Auto<T>`
//! PK, and `RETURNING`. SQLite has no boolean, json, uuid or datetime
//! type, so those all become TEXT or INTEGER.
//!
//! A few operators translate: `ILIKE` becomes
//! `LOWER(col) LIKE LOWER(?)`, and `IS DISTINCT FROM` becomes `IS`.
//! The JSONB containment operators do not, because SQLite's json1
//! extension uses functions rather than operators, so a query using
//! one gets [`SqlError::OperatorNotSupportedInDialect`].

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldType, InsertQuery, Op, SelectQuery, UpdateQuery,
};

use super::writers::{
    write_aggregate, write_bulk_insert, write_bulk_update_sqlite, write_count, write_delete,
    write_insert, write_select, write_update, Sql,
};
use super::{CompiledStatement, Dialect, SqlError};

/// The `strftime` format for the one text shape a SQLite datetime
/// column may hold.
///
/// **SQLite has no datetime type.** A `DateTime<Utc>` column is TEXT
/// and compares as text, so the stored spelling is part of the
/// contract. Two shapes in one column and both `<` and `ORDER BY`
/// give wrong answers.
///
/// This matches what sqlx writes, which is the side that cannot
/// change. `CURRENT_TIMESTAMP` does not: it writes
/// `YYYY-MM-DD HH:MM:SS`, and a space sorts below the `T` in the
/// RFC3339 that sqlx binds, so `WHERE col < ?` was true for every
/// row and a cursor returned its first page forever.
///
/// sqlx's own width varies, emitting 0, 3, 6 or 9 fractional digits,
/// but the family still sorts correctly. `+` sorts below `.` and
/// below every digit, so no fraction comes before any fraction, and
/// a short fraction before a longer one that extends it.
///
/// SQLite's `%f` is `SS.SSS`, so the literal `000` pads its
/// milliseconds out to the six digits this format wants. A DEFAULT
/// therefore has millisecond resolution, SQLite's own limit here,
/// while still sorting against binds of any precision.
pub(crate) const SQLITE_DATETIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%f000+00:00";

/// [`SQLITE_DATETIME_FORMAT`] spelled for chrono instead of SQLite's
/// `strftime`.
///
/// Two strings are needed because the engines disagree on syntax:
/// SQLite's `%f` means seconds plus milliseconds, while chrono's `%S`
/// is seconds alone and `%.6f` is a dot plus six digits. They must
/// produce identical bytes for the same instant, which
/// `the_two_format_spellings_agree` checks.
///
/// **The fixed width is the point.** Left to itself, sqlx emits 0, 3,
/// 6 or 9 fractional digits depending on the value, so the same
/// instant can have two spellings. Equality then finds nothing and
/// `>` finds the row itself, which is how a cursor stops advancing.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) const SQLITE_DATETIME_CHRONO: &str = "%Y-%m-%dT%H:%M:%S%.6f+00:00";

/// Encode a timestamp the one way a SQLite datetime column may hold
/// it. Every write goes through here, so the bind path and the DDL
/// default cannot drift apart.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) fn encode_datetime(d: chrono::DateTime<chrono::Utc>) -> String {
    d.format(SQLITE_DATETIME_CHRONO).to_string()
}

/// A `GLOB` pattern matching [`SQLITE_DATETIME_FORMAT`]'s output and
/// nothing else, so a migration sweep can ask whether a value is
/// already in the canonical shape.
///
/// It has to test the shape rather than round-trip through
/// `strftime`. SQLite's `%f` is milliseconds while the bind path
/// writes microseconds, so `strftime(FMT, col)` is not the identity:
/// `.413681` comes back as `.414000`. A sweep keyed on that would
/// rewrite nearly every correct row, rounding each one forward.
///
/// `?` matches one character and `[0-9]` a digit, so the pattern is
/// exact: the date, `T`, the time, six fraction digits and `+00:00`.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) const SQLITE_CANONICAL_GLOB: &str =
    "[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]+00:00";

/// The `SQLite` 3.35+ dialect. Stateless; construct with `Sqlite`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sqlite;

/// The singleton [`Sqlite`] dialect, which
/// [`crate::sql::Pool::dialect`] hands back for a SQLite pool.
#[cfg(feature = "sqlite")]
pub static DIALECT: &Sqlite = &Sqlite;

impl Dialect for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    // SQLite uses the trait defaults for quoting and placeholders.

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        // `INTEGER PRIMARY KEY AUTOINCREMENT` must stay one phrase:
        // the rowid alias only works with PRIMARY KEY in the type
        // itself. `serial_type_includes_primary_key` below stops the
        // DDL writer adding its own.
        //
        // SQLite stores every integer the same way, so I32 and I64
        // get the same string.
        let _ = field_type;
        "INTEGER PRIMARY KEY AUTOINCREMENT"
    }

    fn serial_type_includes_primary_key(&self) -> bool {
        true
    }

    /// SQLite has no `ALTER TABLE … ADD CONSTRAINT` at all, so every
    /// foreign key has to go inside its `CREATE TABLE`.
    fn inline_fks_in_create_table(&self) -> bool {
        true
    }

    /// SQLite has no `ALTER TABLE … DROP CONSTRAINT`, so there is no
    /// statement to return. Rebuild the table without the constraint
    /// instead.
    fn drop_check_constraint_sql(&self, _table: &str, _name: &str) -> Option<String> {
        None
    }

    /// As [`Sqlite::drop_check_constraint_sql`]: rebuilding the table
    /// is the only way.
    ///
    /// Naming the constraint would not help. There is no statement to
    /// name, whether or not the emitter gave the key a name.
    fn drop_foreign_key_sql(&self, _table: &str, _name: &str) -> Option<String> {
        None
    }

    /// `SQLITE_MAX_VARIABLE_NUMBER`, which is 32766 since SQLite 3.32
    /// and 999 before it. sqlx bundles a modern build. This is half
    /// Postgres' limit, so batches chunk at half the size.
    fn max_bind_params(&self) -> usize {
        32766
    }

    fn supports_returning(&self) -> bool {
        // SQLite 3.35+. The runtime version is not checked, so an
        // older SQLite gives a parse error and must be upgraded.
        true
    }

    /// Translate a Postgres `DEFAULT` expression into SQLite's
    /// spelling.
    ///
    /// - `now()` becomes a `strftime` call in
    ///   [`SQLITE_DATETIME_FORMAT`], **not** `CURRENT_TIMESTAMP`,
    ///   whose output does not sort against the values the bind path
    ///   writes. The parentheses are required: SQLite accepts a
    ///   non-constant DEFAULT only as an expression.
    /// - `'<lit>'::<type>` becomes `'<lit>'`, since SQLite has no
    ///   `::` cast and the bare literal is already right.
    /// - Everything else passes through. `ty` and `max_length` are
    ///   ignored: SQLite takes a literal default on any column.
    fn translate_default_expr(&self, expr: &str, _ty: &str, _max_length: Option<u32>) -> String {
        let trimmed = expr.trim();
        match trimmed {
            "now()" | "NOW()" | "current_timestamp" | "CURRENT_TIMESTAMP" => {
                return format!("(strftime('{SQLITE_DATETIME_FORMAT}','now'))");
            }
            _ => {}
        }
        // Strip a Postgres `::<type>` cast, so `'[]'::jsonb` becomes
        // `'[]'`. `::` cannot appear inside a quoted literal, so
        // searching from the right is safe for the defaults the
        // derive emits.
        if let Some(idx) = trimmed.rfind("::") {
            // Only strip when what follows is a bare identifier, so
            // an expression holding `::` elsewhere survives.
            let suffix = &trimmed[idx + 2..];
            if !suffix.is_empty()
                && suffix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return trimmed[..idx].to_owned();
            }
        }
        expr.to_owned()
    }

    /// SQLite has no `BOOLEAN`; it stores 1 and 0 as integers, so
    /// emit those and match how the value is stored.
    fn bool_literal(&self, b: bool) -> &'static str {
        if b {
            "1"
        } else {
            "0"
        }
    }

    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        // SQLite has no `BIGINT`, so cast to `INTEGER`.
        format!("CAST({expr} AS INTEGER)")
    }

    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        // SQLite uses `REAL` for floating-point.
        format!("CAST({expr} AS REAL)")
    }

    /// SQLite has no `USING` clause; every index is a btree.
    fn index_method_clause(&self, _method: &str) -> String {
        String::new()
    }

    /// A SQLite CAST target is an affinity name: `INTEGER`, `REAL`,
    /// `TEXT`, `NUMERIC` or `BLOB`. Datetimes, UUIDs and JSON are
    /// stored as TEXT, so they cast through TEXT.
    fn cast_type(&self, ty: FieldType) -> Option<&'static str> {
        Some(match ty {
            FieldType::I16 | FieldType::I32 | FieldType::I64 | FieldType::Bool => "INTEGER",
            FieldType::F32 | FieldType::F64 => "REAL",
            FieldType::String
            | FieldType::DateTime
            | FieldType::Date
            | FieldType::Time
            | FieldType::Uuid
            | FieldType::Json => "TEXT",
            FieldType::Decimal => "NUMERIC",
            FieldType::Binary => "BLOB",
            // These are all Postgres-only types with no SQLite
            // equivalent, so they fall back to TEXT.
            FieldType::Array(_) => "TEXT",
            FieldType::Range(_) => "TEXT",
            FieldType::HStore => "TEXT",
            FieldType::Vector(_) => "TEXT",
            FieldType::Geometry(_) => "TEXT",
        })
    }

    /// The `CREATE TABLE` column type, picked from SQLite's five
    /// storage affinities: INTEGER, REAL, TEXT, NUMERIC and BLOB.
    /// A bool is an integer, and a datetime, UUID or JSON value is
    /// text, which is how sqlx round-trips each of them.
    fn column_type(&self, ty: FieldType, max_length: Option<u32>) -> String {
        let _ = max_length; // SQLite puts no length limit on TEXT
        match ty {
            FieldType::I16 | FieldType::I32 | FieldType::I64 => "INTEGER".into(),
            FieldType::F32 | FieldType::F64 => "REAL".into(),
            FieldType::Bool => "INTEGER".into(),
            FieldType::String
            | FieldType::DateTime
            | FieldType::Date
            | FieldType::Time
            | FieldType::Uuid
            | FieldType::Json => "TEXT".into(),
            // `NUMERIC` keeps exact arithmetic: SQLite holds a small
            // value as an integer and a larger one as text.
            FieldType::Decimal => "NUMERIC".into(),
            FieldType::Binary => "BLOB".into(),
            // These are all Postgres-only types with no SQLite
            // equivalent, so the column is TEXT and the bind and
            // decode paths reject them.
            FieldType::Array(_) => "TEXT".into(),
            FieldType::Range(_) => "TEXT".into(),
            FieldType::HStore => "TEXT".into(),
            FieldType::Vector(_) => "TEXT".into(),
            FieldType::Geometry(_) => "TEXT".into(),
        }
    }

    // `COLLATE NOCASE` is built in and makes `=`, `LIKE` and
    // `ORDER BY` case-insensitive with no extension. The collation
    // carries into expressions over the column.
    fn ci_text_type(&self, _max_length: Option<u32>) -> String {
        "TEXT COLLATE NOCASE".to_owned()
    }

    fn supports_op(&self, op: Op) -> bool {
        // SQLite handles every operator lowered below. The JSONB
        // operators are not translated into json1 function calls, so
        // they report "not supported" instead.
        !matches!(
            op,
            Op::JsonContains
                | Op::JsonContainedBy
                | Op::JsonHasKey
                | Op::JsonHasAnyKey
                | Op::JsonHasAllKeys
        )
    }

    /// SQLite has no `ILIKE`, so write
    /// `LOWER(<col>) LIKE LOWER(<placeholder>)`. That folds ASCII
    /// only; other alphabets need the ICU extension.
    fn write_ilike(&self, sql: &mut String, qualified_col: &str, placeholder: &str, negated: bool) {
        if negated {
            sql.push_str("NOT (");
        }
        sql.push_str("LOWER(");
        sql.push_str(qualified_col);
        sql.push_str(") LIKE LOWER(");
        sql.push_str(placeholder);
        sql.push(')');
        if negated {
            sql.push(')');
        }
    }

    /// SQLite's `REGEXP` calls a `regexp(pattern, value)` function
    /// that **you must register**. sqlx does not by default: turn on
    /// its `regexp` feature and use `.with_regexp()`, or register
    /// your own. Without one the query parses but fails at run time
    /// with `no such function: REGEXP`.
    ///
    /// For the case-insensitive form, both sides are lowercased, as
    /// with ILIKE. That folds ASCII only.
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
            sql.push_str("LOWER(");
            sql.push_str(qualified_col);
            sql.push(')');
            sql.push_str(kw);
            sql.push_str("LOWER(");
            sql.push_str(placeholder);
            sql.push(')');
        }
    }

    /// SQLite has no trigram operators, so reject the query here
    /// rather than let the driver fail on it later.
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
            dialect: "sqlite",
        })
    }

    /// SQLite's full-text search lives in FTS5 virtual tables, whose
    /// `MATCH` needs the column to sit on a shadow table. That is a
    /// different schema, so the Postgres shape cannot be translated.
    /// Query the FTS5 table with a raw predicate instead.
    fn write_search(
        &self,
        _sql: &mut String,
        _qualified_col: &str,
        _placeholder: &str,
    ) -> Result<(), super::SqlError> {
        Err(super::SqlError::OpNotSupportedInDialect {
            op: "search (__search) — full-text search shape is Postgres-only; \
                 use SQLite FTS5 `<fts5_table> MATCH ?` via a raw predicate",
            dialect: "sqlite",
        })
    }

    /// SQLite has no array type, so reject every array operator.
    fn write_array_op(
        &self,
        _sql: &mut String,
        _qualified_col: &str,
        _placeholder: &str,
        _op: &'static str,
    ) -> Result<(), super::SqlError> {
        Err(super::SqlError::OpNotSupportedInDialect {
            op: "array operators (@>, <@, &&) — PG ArrayField is Postgres-only; \
                 use JSON-stored arrays + JSON1 functions on SQLite",
            dialect: "sqlite",
        })
    }

    /// SQLite has no range type, so reject every range operator.
    fn write_range_op(
        &self,
        _sql: &mut String,
        _qualified_col: &str,
        _placeholder: &str,
        _op: &'static str,
    ) -> Result<(), super::SqlError> {
        Err(super::SqlError::OpNotSupportedInDialect {
            op: "range operators (@>, <@, &&, <<, >>, -|-) — PG RangeField is Postgres-only; \
                 store lo/hi as separate columns on SQLite",
            dialect: "sqlite",
        })
    }

    /// SQLite's `IS` and `IS NOT` are null-safe comparisons, with
    /// the same meaning as Postgres' `IS [NOT] DISTINCT FROM`.
    fn write_null_safe_eq(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        distinct: bool,
    ) {
        sql.push_str(qualified_col);
        sql.push_str(if distinct { " IS NOT " } else { " IS " });
        sql.push_str(placeholder);
    }

    /// SQLite takes the same `ON CONFLICT … DO NOTHING | DO UPDATE`
    /// shape as Postgres, so this mirrors the Postgres writer.
    fn write_conflict_clause(
        &self,
        sql: &mut String,
        conflict: &ConflictClause,
    ) -> Result<(), SqlError> {
        match conflict {
            ConflictClause::DoNothing => {
                sql.push_str(" ON CONFLICT DO NOTHING");
            }
            ConflictClause::DoUpdate {
                target,
                update_columns,
            } => {
                sql.push_str(" ON CONFLICT (");
                let mut first = true;
                for col in target {
                    if !first {
                        sql.push_str(", ");
                    }
                    first = false;
                    write_sqlite_ident(sql, col);
                }
                sql.push_str(") DO UPDATE SET ");
                let mut first = true;
                for col in update_columns {
                    if !first {
                        sql.push_str(", ");
                    }
                    first = false;
                    write_sqlite_ident(sql, col);
                    sql.push_str(" = excluded.");
                    write_sqlite_ident(sql, col);
                }
            }
        }
        Ok(())
    }

    // ---- compilation: thin shells over `writers::*` ----

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
        let mut b = Sql::new(self);
        write_insert(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_bulk_insert(&self, query: &BulkInsertQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_bulk_insert(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_update(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_bulk_update(&self, query: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError> {
        // SQLite's UPDATE … FROM rejects Postgres' column-list
        // alias on inline VALUES, so use the CTE form instead.
        let mut b = Sql::new(self);
        write_bulk_update_sqlite(&mut b, query)?;
        Ok(b.finish())
    }

    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_delete(&mut b, query)?;
        Ok(b.finish())
    }
}

/// Write a quoted identifier, doubling any embedded quote. It writes
/// in place so the conflict-clause writer allocates no string.
fn write_sqlite_ident(sql: &mut String, name: &str) {
    sql.push('"');
    for c in name.chars() {
        if c == '"' {
            sql.push('"');
        }
        sql.push(c);
    }
    sql.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SqlValue;

    #[test]
    fn name_is_sqlite() {
        assert_eq!(Sqlite.name(), "sqlite");
    }

    #[test]
    fn quote_ident_uses_ansi_double_quotes() {
        // Trait default — confirms we don't override.
        assert_eq!(Sqlite.quote_ident("user_id"), "\"user_id\"");
    }

    #[test]
    fn placeholder_is_question_mark() {
        // Trait default. SQLite also takes `?N`, but the writer
        // already binds in text order, so bare `?` fits.
        assert_eq!(Sqlite.placeholder(1), "?");
        assert_eq!(Sqlite.placeholder(7), "?");
    }

    #[test]
    fn serial_type_is_indivisible_pk_token() {
        assert_eq!(
            Sqlite.serial_type(FieldType::I32),
            "INTEGER PRIMARY KEY AUTOINCREMENT"
        );
        assert_eq!(
            Sqlite.serial_type(FieldType::I64),
            "INTEGER PRIMARY KEY AUTOINCREMENT"
        );
        assert!(Sqlite.serial_type_includes_primary_key());
    }

    #[test]
    fn bool_literal_is_one_or_zero() {
        assert_eq!(Sqlite.bool_literal(true), "1");
        assert_eq!(Sqlite.bool_literal(false), "0");
    }

    #[test]
    fn supports_returning() {
        assert!(Sqlite.supports_returning());
    }

    #[test]
    fn cast_aggregate_uses_sqlite_types() {
        assert_eq!(Sqlite.cast_aggregate_to_int("x"), "CAST(x AS INTEGER)");
        assert_eq!(Sqlite.cast_aggregate_to_float("x"), "CAST(x AS REAL)");
    }

    #[test]
    fn column_type_maps_to_sqlite_affinities() {
        assert_eq!(Sqlite.column_type(FieldType::I16, None), "INTEGER");
        assert_eq!(Sqlite.column_type(FieldType::I32, None), "INTEGER");
        assert_eq!(Sqlite.column_type(FieldType::I64, None), "INTEGER");
        assert_eq!(Sqlite.column_type(FieldType::F32, None), "REAL");
        assert_eq!(Sqlite.column_type(FieldType::F64, None), "REAL");
        assert_eq!(Sqlite.column_type(FieldType::Bool, None), "INTEGER");
        assert_eq!(Sqlite.column_type(FieldType::String, None), "TEXT");
        // VARCHAR length is silently dropped — SQLite ignores it anyway.
        assert_eq!(Sqlite.column_type(FieldType::String, Some(64)), "TEXT");
        assert_eq!(Sqlite.column_type(FieldType::DateTime, None), "TEXT");
        assert_eq!(Sqlite.column_type(FieldType::Date, None), "TEXT");
        assert_eq!(Sqlite.column_type(FieldType::Uuid, None), "TEXT");
        assert_eq!(Sqlite.column_type(FieldType::Json, None), "TEXT");
    }

    #[test]
    fn supports_op_rejects_postgres_jsonb_operators() {
        // The Postgres JSONB operators are not translated.
        assert!(!Sqlite.supports_op(Op::JsonContains));
        assert!(!Sqlite.supports_op(Op::JsonContainedBy));
        assert!(!Sqlite.supports_op(Op::JsonHasKey));
        assert!(!Sqlite.supports_op(Op::JsonHasAnyKey));
        assert!(!Sqlite.supports_op(Op::JsonHasAllKeys));
        // Everything else translates fine.
        assert!(Sqlite.supports_op(Op::Eq));
        assert!(Sqlite.supports_op(Op::ILike));
        assert!(Sqlite.supports_op(Op::IsDistinctFrom));
    }

    #[test]
    fn ilike_lowers_to_lower_like_lower() {
        let mut sql = String::new();
        Sqlite.write_ilike(&mut sql, "\"u\".\"name\"", "?", false);
        assert_eq!(sql, "LOWER(\"u\".\"name\") LIKE LOWER(?)");
        let mut neg = String::new();
        Sqlite.write_ilike(&mut neg, "\"u\".\"name\"", "?", true);
        assert_eq!(neg, "NOT (LOWER(\"u\".\"name\") LIKE LOWER(?))");
    }

    #[test]
    fn null_safe_eq_uses_is_and_is_not() {
        let mut eq = String::new();
        Sqlite.write_null_safe_eq(&mut eq, "\"u\".\"deleted_at\"", "?", false);
        assert_eq!(eq, "\"u\".\"deleted_at\" IS ?");
        let mut neq = String::new();
        Sqlite.write_null_safe_eq(&mut neq, "\"u\".\"deleted_at\"", "?", true);
        assert_eq!(neq, "\"u\".\"deleted_at\" IS NOT ?");
    }

    #[test]
    fn conflict_clause_do_nothing() {
        let mut sql = String::new();
        Sqlite
            .write_conflict_clause(&mut sql, &ConflictClause::DoNothing)
            .unwrap();
        assert_eq!(sql, " ON CONFLICT DO NOTHING");
    }

    #[test]
    fn conflict_clause_do_update_uses_excluded_alias() {
        let mut sql = String::new();
        Sqlite
            .write_conflict_clause(
                &mut sql,
                &ConflictClause::DoUpdate {
                    target: vec!["user_id", "codename"],
                    update_columns: vec!["granted"],
                },
            )
            .unwrap();
        assert_eq!(
            sql,
            " ON CONFLICT (\"user_id\", \"codename\") DO UPDATE SET \"granted\" = excluded.\"granted\""
        );
    }

    #[test]
    fn compile_select_smoke_test() {
        // Build a minimal SelectQuery and assert the SQL emission
        // looks like SQLite.
        use crate::core::{ModelSchema, ModelScope, SelectQuery, WhereExpr};
        static FIELDS: &[crate::core::FieldSchema] = &[crate::core::FieldSchema {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
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
        }];
        static MODEL: ModelSchema = ModelSchema {
            name: "demo",
            table: "demo",
            fields: FIELDS,
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
            scope: ModelScope::Tenant,
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
        };
        let q = SelectQuery {
            model: &MODEL,
            where_clause: WhereExpr::and_predicates(vec![crate::core::Filter {
                column: "id",
                op: Op::Eq,
                value: SqlValue::I64(7),
            }]),
            order_by: vec![],
            joins: vec![],
            subquery_joins: vec![],
            limit: None,
            offset: None,
            search: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
            compound_order_by: vec![],
            compound_limit: None,
            compound_offset: None,
        };
        let stmt = Sqlite.compile_select(&q).unwrap();
        // SQLite emits ANSI-quoted identifiers and `?` placeholders.
        assert!(stmt.sql.contains("\"demo\""), "table quoted: {}", stmt.sql);
        assert!(stmt.sql.contains("\"id\" = ?"), "predicate: {}", stmt.sql);
        assert_eq!(stmt.params.len(), 1);
    }

    /// The encoder must produce a fixed width for every input.
    /// sqlx's own is variable, which is what once made a stored
    /// timestamp differ from its own re-bound form.
    #[test]
    fn the_encoder_is_fixed_width_at_every_precision() {
        use chrono::{TimeZone as _, Utc};
        for ns in [0, 1_000, 123_000_000, 869_000_000, 413_181_000, 999_999_000] {
            let d = Utc.timestamp_opt(1_800_000_000, ns).unwrap();
            let s = encode_datetime(d);
            assert_eq!(
                s.len(),
                32,
                "every encoding must be 32 chars or two instants of different \
                 precision cannot be compared: {ns}ns gave {s}"
            );
            assert!(s.ends_with("+00:00"), "offset must be explicit: {s}");
        }
    }

    /// The `strftime` and chrono spellings must produce the same
    /// bytes, or the DDL default and the bind path drift apart
    /// without anything saying so.
    ///
    /// The check is on the output shape, not the two format strings:
    /// those are deliberately different, so comparing them would
    /// prove nothing.
    #[test]
    fn the_two_format_spellings_agree() {
        use chrono::{TimeZone as _, Utc};
        // What chrono writes for a whole-millisecond instant, the
        // only precision SQLite's `%f` can express.
        let d = Utc.timestamp_opt(1_800_000_000, 869_000_000).unwrap();
        let chrono_side = encode_datetime(d);
        // What `strftime` writes, reproduced from the format's own
        // structure: `%f` gives `SS.SSS`, the literal `000` pads to six.
        let strftime_side = "2027-01-15T08:00:00.869000+00:00";
        assert_eq!(
            chrono_side, strftime_side,
            "the two spellings of SQLITE_DATETIME_FORMAT disagree; a value \
             written by the DDL default would not equal the same instant \
             bound from Rust"
        );
    }

    /// The test above compares chrono against a hand-typed literal,
    /// so it never reads `SQLITE_DATETIME_FORMAT` and a change to
    /// that spelling would slip past it.
    ///
    /// Running `strftime` needs a live SQLite, which a unit test has
    /// not, so this checks the format's structure instead. The
    /// integration test `the_two_engines_render_the_same_bytes`
    /// runs both engines against a real database.
    #[test]
    fn the_literal_above_still_matches_the_strftime_format() {
        // Derive the shape from the format, so editing the format
        // without editing the literal fails here.
        let f = SQLITE_DATETIME_FORMAT;
        assert!(
            f.starts_with("%Y-%m-%dT%H:%M:%f"),
            "the literal in the test above assumes this prefix: {f}"
        );
        assert!(
            f.ends_with("000+00:00"),
            "the literal assumes `%f` is padded by `000` to six digits \
             and closed with a fixed offset: {f}"
        );
    }
}
