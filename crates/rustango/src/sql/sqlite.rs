//! `SQLite` 3.35+ dialect — ANSI double-quoted identifiers, `?`
//! placeholders, `INTEGER PRIMARY KEY AUTOINCREMENT` for `Auto<T>` PKs,
//! `RETURNING` support, no native boolean / json / uuid / datetime types
//! (all map to TEXT or INTEGER affinities).
//!
//! ## v0.27 Phase 1 status
//!
//! - **Phase 1** (this batch) — `Sqlite` Dialect impl + writer dispatch.
//!   SELECT / COUNT / AGGREGATE / INSERT (with RETURNING) / UPDATE /
//!   DELETE all produce valid SQLite SQL through the
//!   [`crate::sql::writers`] machinery.
//! - **Phase 2** (planned) — `Pool::Sqlite` variant + sqlx `SqlitePool`
//!   integration so the `_pool` family executes against SQLite the way
//!   it does against Postgres / MySQL today.
//! - **Phase 3** (planned) — bi-dialect macro `__rustango_from_sqlite_row`
//!   decoder + `LoadRelatedSqlite` / `FkPkAccess` SQLite-typed
//!   counterparts (mirrors the v0.23 MySQL rollout).
//!
//! Operators that don't have a one-shot SQLite translation today
//! (`ILIKE` lowers to `LOWER(<col>) LIKE LOWER(?)` so case-insensitive
//! search works; `IS DISTINCT FROM` lowers to `IS NOT` / `IS`; the
//! Postgres-flavored JSONB containment operators don't translate —
//! SQLite's json1 extension uses function calls, not operators) surface
//! a clear [`SqlError::OperatorNotSupportedInDialect`] from the writers
//! when a query tries to use them.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldType, InsertQuery, Op, SelectQuery, UpdateQuery,
};

use super::writers::{
    write_aggregate, write_bulk_insert, write_bulk_update_sqlite, write_count, write_delete,
    write_insert, write_select, write_update, Sql,
};
use super::{CompiledStatement, Dialect, SqlError};

/// The `SQLite` 3.35+ dialect. Stateless; construct with `Sqlite`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sqlite;

/// `'static` reference to the singleton [`Sqlite`] dialect, symmetric
/// with [`super::postgres::DIALECT`] / [`super::mysql::DIALECT`]. Used
/// by [`crate::sql::Pool::dialect`] (Phase 2) to hand back a
/// `&'static dyn Dialect` regardless of pool variant.
pub static DIALECT: &Sqlite = &Sqlite;

impl Dialect for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    // ANSI double-quoted identifiers + `?`-style placeholders are the
    // trait defaults; SQLite uses both, no override.

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        // SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT` is an
        // indivisible token — the storage layer alias-rules require
        // PRIMARY KEY in the type itself for the rowid alias to take
        // effect. We override `serial_type_includes_primary_key()`
        // below so the DDL writer skips its own PRIMARY KEY append.
        //
        // Note: SQLite ignores the int-width distinction (everything
        // is a variable-length INTEGER under the hood), so we emit the
        // same string for I32 and I64.
        let _ = field_type;
        "INTEGER PRIMARY KEY AUTOINCREMENT"
    }

    fn serial_type_includes_primary_key(&self) -> bool {
        true
    }

    /// SQLite has no `ALTER TABLE … ADD CONSTRAINT` for foreign keys
    /// (or any other constraint kind). The migration renderer must
    /// fold every FK into the originating CREATE TABLE instead.
    fn inline_fks_in_create_table(&self) -> bool {
        true
    }

    fn supports_returning(&self) -> bool {
        // SQLite ≥ 3.35 (released 2021-03). Lower versions reject the
        // clause; rustango doesn't try to detect the runtime version
        // — operators on ancient SQLite get a parse error and need to
        // upgrade.
        true
    }

    /// Translate Postgres-native `DEFAULT` expressions to SQLite
    /// spelling.
    ///
    /// - `now()` / `CURRENT_TIMESTAMP` → `CURRENT_TIMESTAMP` (SQLite has
    ///   no `now()` function in DDL).
    /// - `'<lit>'::<type>` → `'<lit>'` (SQLite has no `::` cast syntax;
    ///   the bare literal is the right encoding for JSON-as-TEXT,
    ///   boolean-as-INTEGER, etc.).
    /// - Everything else passes through. `_ty` / `_max_length` are
    ///   ignored — SQLite has no per-type DEFAULT syntax quirks and
    ///   accepts literal defaults on every column type.
    fn translate_default_expr(&self, expr: &str, _ty: &str, _max_length: Option<u32>) -> String {
        let trimmed = expr.trim();
        match trimmed {
            "now()" | "NOW()" | "current_timestamp" | "CURRENT_TIMESTAMP" => {
                return "CURRENT_TIMESTAMP".to_owned();
            }
            _ => {}
        }
        // Strip Postgres `::<type>` cast suffix. Common cases:
        //   "'[]'::jsonb"   → "'[]'"
        //   "'{}'::jsonb"   → "'{}'"
        //   "0::int"        → "0"
        // The `::` token can't legally appear inside a single-quoted
        // SQL literal except as part of an escape, so a simple
        // rfind('::') is safe for the well-formed defaults the macro
        // layer emits.
        if let Some(idx) = trimmed.rfind("::") {
            // Guard: only strip if everything after `::` is an identifier
            // (no spaces, parens, etc.) so we don't mangle expressions
            // that legitimately contain `::` outside cast position.
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

    /// SQLite has no native `BOOLEAN`. `INTEGER` 1 / 0 is the
    /// canonical encoding. Emit `1` / `0` so DEFAULT clauses and
    /// inline comparisons match the storage shape.
    fn bool_literal(&self, b: bool) -> &'static str {
        if b {
            "1"
        } else {
            "0"
        }
    }

    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        // SQLite supports `CAST(<expr> AS INTEGER)`. Use that; the
        // ANSI default `BIGINT` is not a recognized SQLite type.
        format!("CAST({expr} AS INTEGER)")
    }

    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        // SQLite uses `REAL` for floating-point.
        format!("CAST({expr} AS REAL)")
    }

    /// SQLite's "type affinities" map onto a small fixed set of
    /// storage classes: INTEGER, REAL, TEXT, BLOB, NUMERIC. We pick
    /// the closest affinity for each rustango `FieldType`:
    /// - `Bool` → `INTEGER` (1/0 encoding; sqlx maps `bool` to
    ///   INTEGER on SQLite).
    /// - `DateTime` / `Date` → `TEXT` (ISO-8601 string; sqlx-sqlite
    ///   uses TEXT round-trip for `chrono::DateTime<Utc>`).
    /// - `Uuid` → `TEXT` (canonical hyphenated form).
    /// - `Json` → `TEXT` (json1 extension stores JSON as TEXT
    ///   internally; the parser sees it as text).
    /// - `String` with `max_length` → `TEXT` (SQLite ignores VARCHAR
    ///   lengths but accepts the keyword; emit `TEXT` for clarity).
    /// SQLite has no `USING` clause — drop the method entirely.
    /// Indexes work as btree on every SQLite engine. Issue #34.
    fn index_method_clause(&self, _method: &str) -> String {
        String::new()
    }

    /// SQLite CAST targets are the affinity names: `INTEGER`, `REAL`,
    /// `TEXT`, `NUMERIC`, `BLOB`. Datetime/UUID/JSON have no native
    /// CAST target — they store as TEXT, so cast routes through TEXT.
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
            // SQLite has no array type — `Array<T>` (#341) is PG-only.
            // Degrade the CAST target to TEXT affinity.
            FieldType::Array(_) => "TEXT",
            // Ditto for PG range columns (#343).
            FieldType::Range(_) => "TEXT",
            // Ditto for PG hstore columns (#342).
            FieldType::HStore => "TEXT",
            // Ditto for pgvector `vector` columns (#824).
            FieldType::Vector(_) => "TEXT",
            // Ditto for PostGIS `geometry` columns (#443).
            FieldType::Geometry(_) => "TEXT",
        })
    }

    fn column_type(&self, ty: FieldType, max_length: Option<u32>) -> String {
        let _ = max_length; // SQLite has no length constraint on TEXT
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
            // `NUMERIC` affinity: SQLite stores small values as
            // INTEGER, larger as TEXT, preserving exact arithmetic.
            // `rust_decimal::Decimal` round-trips through `sqlx`'s
            // text encoding on this affinity.
            FieldType::Decimal => "NUMERIC".into(),
            // `BLOB` storage class — round-trips `Vec<u8>` directly.
            FieldType::Binary => "BLOB".into(),
            // SQLite has no native array type — `Array<T>` (#341) is
            // PG-only by language semantics. Degrade to `TEXT`; the
            // bind / decode paths error on SQLite.
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

    // #344 — CITextField. SQLite has built-in `COLLATE NOCASE` that
    // makes `=` / `LIKE` / `ORDER BY` case-insensitive without an
    // extension. Column-level collation propagates to expressions
    // referencing the column.
    fn ci_text_type(&self, _max_length: Option<u32>) -> String {
        "TEXT COLLATE NOCASE".to_owned()
    }

    fn supports_op(&self, op: Op) -> bool {
        // SQLite supports every operator we lower below. The Postgres-
        // shape JSONB operators are intentionally NOT translated to
        // json1 function calls — that's a Phase 2+ feature; today
        // they surface a clear "not supported" error.
        !matches!(
            op,
            Op::JsonContains
                | Op::JsonContainedBy
                | Op::JsonHasKey
                | Op::JsonHasAnyKey
                | Op::JsonHasAllKeys
        )
    }

    /// SQLite has no native `ILIKE`. Lower to
    /// `LOWER(<col>) LIKE LOWER(<placeholder>)` — handles ASCII
    /// case-insensitivity. Unicode case folding requires the
    /// `ICU` extension; outside scope here.
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

    /// SQLite's `REGEXP` operator delegates to a user-defined
    /// `regexp(pattern, value)` function. sqlx-sqlite does **not**
    /// register one by default — callers either enable sqlx-sqlite's
    /// `regexp` cargo feature (which adds a `.with_regexp()` builder
    /// on `SqliteConnectOptions`) or register their own via
    /// `SqliteConnection::lock_handle()` + raw FFI. Without one
    /// registered, the query fails at execution with `no such
    /// function: REGEXP` (parser-clean, runtime-only). For case-
    /// insensitive matching we mirror the ILIKE strategy: lowercase
    /// both sides so the comparison is collation-independent. ASCII-
    /// only folding; non-ASCII patterns may not behave as expected
    /// without the ICU extension. Issue #26.
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

    /// `pg_trgm` trigram operators are Postgres-only. SQLite has no
    /// equivalent. Reject at compile time so the user retargets the
    /// query rather than seeing a driver-level syntax error at run
    /// time. Issue #29.
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

    /// Postgres-shape FTS (`to_tsvector @@ plainto_tsquery`) doesn't
    /// translate to SQLite. SQLite's FTS lives in FTS5 virtual tables
    /// with a `MATCH` operator that requires the column to live on an
    /// FTS5-shadow table — a different schema shape entirely. Reject
    /// at compile time so the user either retargets the backend or
    /// queries the FTS5 shadow table via a raw predicate. Issue #28.
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

    /// SQLite has no native array type. Reject every PG array op at
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
                 use JSON-stored arrays + JSON1 functions on SQLite",
            dialect: "sqlite",
        })
    }

    /// SQLite has no native range type. Reject every PG range op at
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
                 store lo/hi as separate columns on SQLite",
            dialect: "sqlite",
        })
    }

    /// SQLite's `IS` / `IS NOT` are null-safe equality / inequality
    /// (both `NULL IS NULL` and `1 IS 1` evaluate to true). Same
    /// semantics as Postgres' `IS [NOT] DISTINCT FROM` — we just
    /// emit the SQLite spelling.
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

    /// SQLite supports the same `ON CONFLICT (target) DO NOTHING |
    /// DO UPDATE SET ...` shape as Postgres (via the json1 / upsert
    /// extensions, both bundled in modern builds). The shape matches
    /// 1:1 so we mirror the Postgres writer.
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
        // #560 — SQLite's UPDATE-FROM doesn't accept the
        // column-list-alias-on-inline-VALUES form Postgres uses
        // (`FROM (VALUES …) AS __data(pk, col, …)` → `near "(":
        // syntax error`). Route to a CTE + correlated-subquery
        // shape that parses on every SQLite that supports CTEs
        // (3.8.3, 2014); see `writers::write_bulk_update_sqlite`.
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

/// SQLite identifier writer — same shape as Postgres' (ANSI double
/// quotes, embedded `"` doubled). Pulled out so the conflict-clause
/// writer doesn't have to allocate a new String per identifier.
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
        // Trait default. SQLite also accepts `?N` (1-based) but the
        // sequential `?` form lines up cleanly with the existing
        // writer machinery — the writer already binds parameters in
        // order.
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
        // Phase 1 doesn't translate Postgres-flavored JSONB operators.
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
        };
        let stmt = Sqlite.compile_select(&q).unwrap();
        // SQLite emits ANSI-quoted identifiers and `?` placeholders.
        assert!(stmt.sql.contains("\"demo\""), "table quoted: {}", stmt.sql);
        assert!(stmt.sql.contains("\"id\" = ?"), "predicate: {}", stmt.sql);
        assert_eq!(stmt.params.len(), 1);
    }
}
