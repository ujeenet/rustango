//! The `Dialect` trait — one implementation per database backend.
//!
//! v0.8 promotes the trait from a query-compiler-only surface into the
//! per-dialect seam every SQL writer dispatches through. Postgres is
//! the only built-in `impl` shipped today; SQLite + MySQL slot in via
//! v0.10's slice 10.5 by adding new `impl Dialect for SqliteDialect`
//! and `impl Dialect for MySqlDialect` blocks alongside.
//!
//! The methods split into three layers:
//!
//! * **Compilation** — `compile_select` / `_insert` / `_update` /
//!   `_delete` / `_count` / `_bulk_insert`. Lower the dialect-neutral
//!   query IR (`SelectQuery`, etc.) to a parameterized
//!   [`CompiledStatement`]. Always overridden.
//! * **DDL primitives** — `quote_ident`, `placeholder`, `serial_type`,
//!   `bool_literal`, `supports_concurrent_index`, `supports_returning`.
//!   Used by `migrate::ddl` when emitting `CREATE TABLE` and friends.
//!   Most have sensible defaults (ANSI-quoted identifiers, `?`
//!   placeholders, no `RETURNING`); Postgres overrides what differs.
//! * **Identity** — `name()` for diagnostic logging.
//!
//! v0.10 will add advisory-lock helpers (`with_session_lock`,
//! `with_xact_lock`) so `migrate::runner` can dispatch through the
//! dialect instead of the current Postgres-typed direct calls.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldType, InsertQuery, Op, SelectQuery, UpdateQuery,
};

use super::{CompiledStatement, SqlError};

/// Postgres-shape `<col> ?| ARRAY[$1, $2, …]` / `<col> ?& ARRAY[…]`
/// helper. Factored out so both default JSON-key methods call it.
fn write_pg_array_keys(
    sql: &mut String,
    qualified_col: &str,
    placeholders: &[String],
    keyword: &str,
) {
    sql.push_str(qualified_col);
    sql.push_str(keyword);
    let mut first = true;
    for p in placeholders {
        if !first {
            sql.push_str(", ");
        }
        first = false;
        sql.push_str(p);
    }
    sql.push(']');
}

/// Writes a dialect-neutral query IR to a parameterized statement,
/// plus the small bag of per-dialect DDL primitives the migration
/// runner needs (identifier quoting, placeholder syntax, `SERIAL` /
/// `AUTOINCREMENT` spelling, etc.).
pub trait Dialect {
    // ====== Identity ======

    /// Short identifier for this dialect — `"postgres"`, `"sqlite"`,
    /// `"mysql"`. Used in error messages and tracing spans only;
    /// callers should not branch on the value.
    fn name(&self) -> &'static str;

    // ====== DDL primitives (default-implemented to ANSI shape) ======

    /// Quote an identifier (table or column name) with the dialect's
    /// quoting rules. Default: ANSI double-quotes — works for
    /// Postgres + SQLite; MySQL overrides to backticks. Embedded
    /// quote characters are doubled so the result is always safe.
    fn quote_ident(&self, name: &str) -> String {
        let escaped = name.replace('"', "\"\"");
        format!("\"{escaped}\"")
    }

    /// Render the `n`-th positional parameter placeholder. `n` is
    /// 1-based. Default: `?` — works for SQLite + MySQL; Postgres
    /// overrides to `$N`.
    fn placeholder(&self, n: usize) -> String {
        let _ = n;
        "?".to_owned()
    }

    /// SQL column type for an auto-incrementing PK declared as
    /// `Auto<T>` in the model. `field_type` is `FieldType::I32` or
    /// `FieldType::I64`; non-integer types hit the default path
    /// (`INTEGER` / `BIGINT`) — the macro layer rejects `Auto<T>` on
    /// non-integers anyway.
    ///
    /// Default: ANSI-leaning `BIGINT` / `INTEGER`. Postgres overrides
    /// to `BIGSERIAL` / `SERIAL`; SQLite to `INTEGER PRIMARY KEY
    /// AUTOINCREMENT`; MySQL to `BIGINT AUTO_INCREMENT`.
    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "INTEGER",
            _ => "BIGINT",
        }
    }

    /// `true` when [`Self::serial_type`]'s output already contains the
    /// `PRIMARY KEY` clause inline — SQLite's case, where the storage
    /// layer requires `INTEGER PRIMARY KEY AUTOINCREMENT` as one
    /// indivisible token. Default `false`; the DDL writer separately
    /// appends `PRIMARY KEY` for backends that don't bake it in.
    fn serial_type_includes_primary_key(&self) -> bool {
        false
    }

    /// `true` when the migration renderer should emit FK clauses inline
    /// inside `CREATE TABLE` (column-level `REFERENCES …` plus table-
    /// level `FOREIGN KEY (…) REFERENCES …` for composites) instead of
    /// as post-hoc `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` ops.
    ///
    /// Default `false` (Postgres/MySQL: defer FKs so circular table
    /// references resolve cleanly across the whole batch). SQLite
    /// overrides to `true` — it has no `ALTER TABLE … ADD CONSTRAINT`
    /// support, so FKs must live inside the originating CREATE.
    fn inline_fks_in_create_table(&self) -> bool {
        false
    }

    /// Translate a `DEFAULT` expression authored in Postgres dialect
    /// (the rustango canonical form) to the target backend's spelling.
    /// `ty` is the field's `FieldType` token (e.g. `"json"`, `"datetime"`,
    /// `"string"`) so dialects can choose syntax per column type — MySQL
    /// in particular needs `DEFAULT ('{}')` (expression default) for
    /// `JSON` columns since `DEFAULT '{}'` is rejected.
    ///
    /// Default: pass-through (Postgres-native). SQLite overrides
    /// `now()` → `CURRENT_TIMESTAMP` and strips `::type` casts.
    /// MySQL overrides similarly plus wraps defaults on its
    /// LOB-rendered columns (JSON / TEXT / BLOB) in parens.
    ///
    /// `max_length` is the field's declared length cap (PG/SQLite
    /// ignore it here); MySQL needs it to tell an unbounded `String`
    /// (→ `TEXT`, which forbids a literal `DEFAULT`) apart from a
    /// bounded one (→ `VARCHAR(n)`, which allows it).
    fn translate_default_expr(&self, expr: &str, _ty: &str, _max_length: Option<u32>) -> String {
        expr.to_owned()
    }

    /// Render a boolean literal for `DEFAULT` clauses and inline
    /// comparisons. Default: ANSI `TRUE` / `FALSE`. SQLite + MySQL
    /// override to `1` / `0` (no native boolean type).
    fn bool_literal(&self, b: bool) -> &'static str {
        if b {
            "TRUE"
        } else {
            "FALSE"
        }
    }

    /// `true` if `CREATE INDEX CONCURRENTLY` is honored. Postgres
    /// overrides to `true`; default is `false` so other dialects
    /// silently downgrade `atomic: false` migrations to a regular
    /// `CREATE INDEX` with a warning.
    fn supports_concurrent_index(&self) -> bool {
        false
    }

    /// `true` if `CREATE INDEX IF NOT EXISTS` is accepted. Postgres
    /// and SQLite support the guard; MySQL does not (as of 8.x). The
    /// migration renderer omits the `IF NOT EXISTS` token when this
    /// returns `false`; the ledger already prevents re-runs so the
    /// guard is belt-and-suspenders.
    fn supports_create_index_if_not_exists(&self) -> bool {
        true
    }

    /// Render the `USING <method>` clause appended after the table
    /// name in `CREATE INDEX … ON tbl <USING …> (cols)`. Returns
    /// an empty string for the default `btree` method (the standard
    /// "no clause" shape) and for dialects that lack `USING`
    /// support entirely (SQLite). The dialect implementation is
    /// responsible for sanitizing the method token — unsupported
    /// methods on a backend should degrade to `""` so the index
    /// still gets created as btree. Issue #34.
    fn index_method_clause(&self, method: &str) -> String {
        // PG default: emit `USING <method>` for any non-default
        // method. The dialect-specific overrides further restrict
        // (e.g. SQLite always empty, MySQL only btree/hash).
        match method {
            "" | "btree" => String::new(),
            other => format!(" USING {other}"),
        }
    }

    /// Render the "insert-or-skip" tail clause appended to `INSERT INTO
    /// … VALUES (…)` so the caller's row inserts cleanly on first run
    /// and is silently skipped on re-run.
    ///
    /// `conflict_cols` are the **already-quoted** column identifiers
    /// that define the unique constraint the caller is targeting
    /// (typically a `UNIQUE INDEX` or composite PK).
    ///
    /// Defaults to Postgres/SQLite `ON CONFLICT (…) DO NOTHING`. MySQL
    /// overrides to `ON DUPLICATE KEY UPDATE <col0> = <col0>` (no-op
    /// write — satisfies MySQL's syntax requirement for at least one
    /// SET expression while leaving the existing row untouched).
    fn insert_on_conflict_skip(&self, conflict_cols: &[&str]) -> String {
        if conflict_cols.is_empty() {
            return String::new();
        }
        format!("ON CONFLICT ({}) DO NOTHING", conflict_cols.join(", "))
    }

    /// `true` if `INSERT ... RETURNING <cols>` is honored. Postgres
    /// always; SQLite ≥ 3.35; MySQL never. Drives the macro
    /// codegen's `Auto<T>` insert path: `false` here forces the
    /// runner to do a `last_insert_id()`-style follow-up read.
    fn supports_returning(&self) -> bool {
        false
    }

    /// `true` if the dialect understands the `ORDER BY … NULLS
    /// FIRST|LAST` keyword (issue #76). Postgres + SQLite yes;
    /// MySQL no — there the writer emulates `NULLS FIRST/LAST`
    /// with an `<col> IS NULL` pre-sort term:
    ///
    /// - `NULLS FIRST` on `ASC`: emit `<col> IS NULL DESC, <col> ASC`
    /// - `NULLS LAST`  on `ASC`: emit `<col> IS NULL ASC,  <col> ASC`
    /// - same pattern for DESC.
    fn supports_nulls_order(&self) -> bool {
        true
    }

    /// SQL identifier for the dialect's "random number" function used
    /// by `ORDER BY RANDOM()` (issue #77). PG + SQLite expose
    /// `RANDOM()`; MySQL is the outlier with `RAND()`. Default:
    /// `RANDOM` (covers PG + SQLite; MySQL overrides).
    fn random_fn(&self) -> &'static str {
        "RANDOM"
    }

    /// SQL fragment to emit as the `LIMIT` clause when the query has
    /// an explicit `OFFSET` but no `LIMIT` (#560).
    ///
    /// MySQL's grammar requires a `LIMIT` whenever `OFFSET` is used
    /// — `SELECT … OFFSET 10` alone is `ERROR 1064` ("you have an
    /// error in your SQL syntax"). The documented workaround is to
    /// pair the OFFSET with the max-`u64` literal:
    /// `LIMIT 18446744073709551615 OFFSET 10`. PG + SQLite accept
    /// bare `OFFSET` without a `LIMIT`, so they return `None` here
    /// and the writer emits the offset directly.
    ///
    /// Returning `Some("…")` causes the writer to prepend the
    /// fragment ahead of the `OFFSET` clause; returning `None`
    /// (the default) means "no placeholder LIMIT required".
    fn offset_without_limit_clause(&self) -> Option<&'static str> {
        None
    }

    /// Wrap a SUM expression in a cast back to BIGINT. PostgreSQL's
    /// SUM(BIGINT) is NUMERIC and MySQL's is DECIMAL — both fall
    /// through to Null in the aggregate row decoder which only tries
    /// scalar types. Default: ANSI `CAST(<expr> AS BIGINT)`.
    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        format!("CAST({expr} AS BIGINT)")
    }

    /// Wrap an AVG expression in a cast to a floating-point type.
    /// PostgreSQL AVG of any int returns NUMERIC; MySQL AVG returns
    /// DOUBLE for ints, DECIMAL for decimals. Default: ANSI
    /// `CAST(<expr> AS DOUBLE PRECISION)`.
    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        format!("CAST({expr} AS DOUBLE PRECISION)")
    }

    /// SQL type to cast a `NULL` parameter to when the column type is
    /// known. Postgres needs this — `INSERT INTO t(name) VALUES ($1)`
    /// with `$1 = NULL` against an integer column raises
    /// `column "x" is of type integer but expression is of type text`,
    /// and the cast (`$1::INTEGER`) tells Postgres exactly which NULL
    /// we mean. `MySQL` has no equivalent issue — return `None` and the
    /// writer skips the cast. Default: `None`.
    fn null_cast(&self, ty: FieldType) -> Option<&'static str> {
        let _ = ty;
        None
    }

    /// Whether `CREATE [UNIQUE] INDEX ... WHERE <expr>` partial-index
    /// syntax is supported. PG and SQLite (3.8+) both ship it natively;
    /// MySQL has no equivalent (the migration writer drops the WHERE
    /// clause + emits a warning so the rest of the migration still
    /// applies). Default `true`; MySQL overrides to `false`. Issue
    /// #265 / T1.3.
    fn supports_partial_index(&self) -> bool {
        true
    }

    /// Django-parity `Meta.required_db_features` capability query — does
    /// this dialect advertise the given token? `manage check --deploy`
    /// walks every model's `required_db_features` and warns when this
    /// returns `false`, so projects can declare "needs PG `LISTEN/NOTIFY`"
    /// or "needs MySQL 8 CTE support" and get a deploy-time signal
    /// rather than a runtime surprise.
    ///
    /// Default impl whitelists tokens supported by all three backends
    /// rustango ships (`window_functions`, `recursive_cte`,
    /// `json_extract`, `expression_index`, `partial_index` —
    /// the last gated on [`Self::supports_partial_index`]). Per-dialect
    /// impls extend with PG-specific (`array_type`, `range_type`,
    /// `hstore`, `citext`, `listen_notify`, `row_security`, `gin_index`,
    /// `gist_index`, `unique_constraint_deferred`) or MySQL/SQLite
    /// specifics.
    ///
    /// Unknown tokens return `false` (treat as "not supported" so the
    /// deploy warning fires; that's the safer default for aspirational
    /// declarations).
    #[must_use]
    fn supports(&self, token: &str) -> bool {
        self.default_supports(token)
    }

    /// Tokens supported by all three backends rustango ships against —
    /// per-dialect overrides of [`Self::supports`] call this for the
    /// generic baseline and then `||`-add their specifics.
    #[must_use]
    fn default_supports(&self, token: &str) -> bool {
        matches!(
            token,
            "window_functions" | "recursive_cte" | "cte" | "json_extract" | "expression_index"
        ) || (token == "partial_index" && self.supports_partial_index())
            || (token == "returning" && self.supports_returning())
    }

    /// Dialect-specific SQL type token for the rhs of `CAST(<expr> AS <ty>)`
    /// — distinct from [`Self::null_cast`] (which is PG-specific) and
    /// [`Self::column_type`] (DDL, which includes lengths like
    /// `VARCHAR(N)`). Returns `None` when the dialect genuinely can't
    /// cast to that `FieldType` (e.g. SQLite has no native UUID /
    /// JSONB types — caller should error).
    ///
    /// Default returns the Postgres-standard token names (`BIGINT`,
    /// `TEXT`, `TIMESTAMPTZ`, …). Backends with divergent CAST grammar
    /// (MySQL's `SIGNED`/`UNSIGNED` for ints, no `CAST AS JSON`, etc.)
    /// override.
    fn cast_type(&self, ty: FieldType) -> Option<&'static str> {
        Some(match ty {
            FieldType::I16 => "SMALLINT",
            FieldType::I32 => "INTEGER",
            FieldType::I64 => "BIGINT",
            FieldType::F32 => "REAL",
            FieldType::F64 => "DOUBLE PRECISION",
            FieldType::Bool => "BOOLEAN",
            FieldType::String => "TEXT",
            FieldType::DateTime => "TIMESTAMPTZ",
            FieldType::Date => "DATE",
            FieldType::Uuid => "UUID",
            FieldType::Json => "JSONB",
            FieldType::Decimal => "NUMERIC",
            FieldType::Binary => "BYTEA",
            FieldType::Time => "TIME",
            // PG array CAST targets (#341).
            FieldType::Array(crate::core::ArrayElem::Text) => "text[]",
            FieldType::Array(crate::core::ArrayElem::Int) => "integer[]",
            FieldType::Array(crate::core::ArrayElem::BigInt) => "bigint[]",
            // PG range CAST targets (#343).
            FieldType::Range(crate::core::RangeElem::Int) => "int4range",
            FieldType::Range(crate::core::RangeElem::BigInt) => "int8range",
            FieldType::Range(crate::core::RangeElem::Numeric) => "numrange",
            FieldType::Range(crate::core::RangeElem::Date) => "daterange",
            FieldType::Range(crate::core::RangeElem::DateTime) => "tstzrange",
        })
    }

    /// SQL column type for a non-`Auto<T>` field, used by the DDL
    /// writer (`CREATE TABLE`). `max_length` is honored on
    /// [`FieldType::String`] (`VARCHAR(N)` vs unbounded text).
    ///
    /// Default emits Postgres-shape names (`BIGINT`, `BOOLEAN`,
    /// `TEXT`, `TIMESTAMPTZ`, `JSONB`, `UUID`). `MySQL` overrides
    /// because:
    /// - `BOOLEAN` is `TINYINT(1)`
    /// - `TEXT` works but `VARCHAR` requires explicit length
    /// - `TIMESTAMPTZ` doesn't exist; use `DATETIME(6)` or `TIMESTAMP(6)`
    /// - `JSONB` doesn't exist; use `JSON`
    /// - `UUID` doesn't exist; use `CHAR(36)` (string) or `BINARY(16)` (compact)
    fn column_type(&self, ty: FieldType, max_length: Option<u32>) -> String {
        match ty {
            FieldType::I16 => "SMALLINT".into(),
            FieldType::I32 => "INTEGER".into(),
            FieldType::I64 => "BIGINT".into(),
            FieldType::F32 => "REAL".into(),
            FieldType::F64 => "DOUBLE PRECISION".into(),
            FieldType::Bool => "BOOLEAN".into(),
            FieldType::String => match max_length {
                Some(n) => format!("VARCHAR({n})"),
                None => "TEXT".into(),
            },
            FieldType::DateTime => "TIMESTAMPTZ".into(),
            FieldType::Date => "DATE".into(),
            FieldType::Time => "TIME".into(),
            FieldType::Uuid => "UUID".into(),
            FieldType::Json => "JSONB".into(),
            FieldType::Decimal => "NUMERIC".into(),
            FieldType::Binary => "BYTEA".into(),
            // Native PG array column — `text[]` / `integer[]` /
            // `bigint[]` (#341). `max_length` is ignored (arrays carry
            // no length cap at the type level).
            FieldType::Array(elem) => format!("{}[]", elem.pg_element_type()),
            // Native PG range column (#343).
            FieldType::Range(elem) => elem.pg_range_type().to_owned(),
        }
    }

    /// Column type for a case-insensitive text field (Django parity
    /// #344 — `CITextField` / `case_insensitive = true`). Default
    /// emits ANSI `TEXT` so unknown dialects degrade gracefully (case
    /// sensitivity is then a query-side concern). Postgres overrides
    /// to `CITEXT`, SQLite to `TEXT COLLATE NOCASE`, MySQL to
    /// `<VARCHAR(N)|TEXT> COLLATE utf8mb4_general_ci`.
    fn ci_text_type(&self, max_length: Option<u32>) -> String {
        // ANSI fallback — caller is expected to use `LOWER(...)` at
        // query time when running on a dialect that doesn't override.
        let _ = max_length;
        "TEXT".to_owned()
    }

    /// One-time DDL prelude this dialect needs before any column of
    /// the given type can be created (e.g. `CREATE EXTENSION IF NOT
    /// EXISTS citext;` on Postgres before a `CITEXT` column can
    /// land). `None` (default) means no prelude required.
    fn ci_text_extension_sql(&self) -> Option<&'static str> {
        None
    }

    /// `true` if `op` can be lowered to SQL by this dialect. Default
    /// `true` — every dialect ships translations for every operator
    /// in the IR. Override to return `false` for ops that genuinely
    /// have no equivalent (rare); the writer surfaces a clear
    /// [`SqlError::OperatorNotSupportedInDialect`] in that case.
    fn supports_op(&self, op: Op) -> bool {
        let _ = op;
        true
    }

    // ---- per-operator predicate writers ----
    //
    // Each method is handed the **already-rendered, qualified column**
    // (e.g. `"users"."name"` on Postgres or `` `users`.`name` `` on
    // MySQL) plus a placeholder string (e.g. `$1` or `?`). The dialect
    // composes them into the SQL fragment its parser expects. Default
    // implementations emit Postgres / ANSI shape; MySQL overrides the
    // ones that need a different translation.

    /// Case-insensitive LIKE: `<col> ILIKE <p>` (Postgres) or
    /// `LOWER(<col>) LIKE LOWER(<p>)` (MySQL fallback).
    fn write_ilike(&self, sql: &mut String, qualified_col: &str, placeholder: &str, negated: bool) {
        sql.push_str(qualified_col);
        sql.push_str(if negated { " NOT ILIKE " } else { " ILIKE " });
        sql.push_str(placeholder);
    }

    /// POSIX regex match — Django `__regex` / `__iregex`. Issue #26.
    ///
    /// Default emits the Postgres shape:
    /// - `case_sensitive=true`,  `negated=false`: `<col> ~ <p>`
    /// - `case_sensitive=true`,  `negated=true`:  `<col> !~ <p>`
    /// - `case_sensitive=false`, `negated=false`: `<col> ~* <p>`
    /// - `case_sensitive=false`, `negated=true`:  `<col> !~* <p>`
    ///
    /// MySQL and SQLite override to use the `REGEXP` / `NOT REGEXP`
    /// keyword. Neither has a native case-insensitive regex operator,
    /// so both wrap `<col>` and `<placeholder>` in `LOWER(...)` for
    /// the case-insensitive variants. The pattern is bound as a
    /// single string parameter — `placeholder` is the slot it goes
    /// into; the dialect doesn't rewrite the bound value.
    fn write_regex(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        case_sensitive: bool,
        negated: bool,
    ) {
        sql.push_str(qualified_col);
        let op = match (case_sensitive, negated) {
            (true, false) => " ~ ",
            (true, true) => " !~ ",
            (false, false) => " ~* ",
            (false, true) => " !~* ",
        };
        sql.push_str(op);
        sql.push_str(placeholder);
    }

    /// `pg_trgm` trigram similarity match — Django's
    /// `__trigram_similar` / `__trigram_word_similar`. Issue #29.
    ///
    /// Default emits the Postgres shape:
    /// - `word=false`: `<col> % <p>` (whole-string similarity)
    /// - `word=true`:  `<col> %> <p>` (any word similar)
    ///
    /// Requires `CREATE EXTENSION pg_trgm` on the database.
    ///
    /// **MySQL and SQLite have no equivalent** — they override to
    /// return [`SqlError::OpNotSupportedInDialect`] so the typo of
    /// using a trigram lookup against the wrong backend surfaces
    /// cleanly at compile time rather than as a driver syntax error.
    ///
    /// # Errors
    /// `OpNotSupportedInDialect` when the dialect doesn't support
    /// trigram operators.
    fn write_trigram_similar(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        word: bool,
    ) -> Result<(), super::SqlError> {
        sql.push_str(qualified_col);
        sql.push_str(if word { " %> " } else { " % " });
        sql.push_str(placeholder);
        Ok(())
    }

    /// Postgres full-text search — Django's `__search` lookup. Issue
    /// #28.
    ///
    /// Default emits the simplest portable shape:
    /// `to_tsvector(<col>) @@ plainto_tsquery(<p>)`. The database
    /// applies its `default_text_search_config` (typically `english`).
    ///
    /// **MySQL and SQLite** have their own FTS shapes (`MATCH … AGAINST`
    /// on MySQL, FTS5 virtual-table `MATCH` on SQLite) with
    /// incompatible semantics — they override to return
    /// [`SqlError::OpNotSupportedInDialect`] so the typo of using
    /// `__search` against the wrong backend surfaces cleanly at
    /// compile time rather than as a driver syntax error.
    ///
    /// # Errors
    /// `OpNotSupportedInDialect` when the dialect doesn't support
    /// Postgres-shape FTS.
    fn write_search(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
    ) -> Result<(), super::SqlError> {
        sql.push_str("to_tsvector(");
        sql.push_str(qualified_col);
        sql.push_str(") @@ plainto_tsquery(");
        sql.push_str(placeholder);
        sql.push(')');
        Ok(())
    }

    /// Postgres `ArrayField` containment / overlap operators —
    /// `@>` / `<@` / `&&`. Issue #30.
    ///
    /// `op` is the literal SQL operator string (`"@>"`, `"<@"`,
    /// `"&&"`). The default impl emits the PG shape
    /// `<col> <op> <placeholder>`. MySQL + SQLite override to
    /// reject with [`SqlError::OpNotSupportedInDialect`] since
    /// neither has a native array type.
    ///
    /// # Errors
    /// `OpNotSupportedInDialect` when the dialect doesn't support
    /// PG array operators.
    fn write_array_op(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        op: &'static str,
    ) -> Result<(), super::SqlError> {
        sql.push_str(qualified_col);
        sql.push(' ');
        sql.push_str(op);
        sql.push(' ');
        sql.push_str(placeholder);
        Ok(())
    }

    /// Postgres `RangeField` operators — `@>` / `<@` / `&&` / `<<` /
    /// `>>` / `-|-`. Issue #31.
    ///
    /// `op` is the literal SQL operator. The default emits the PG
    /// shape `<col> <op> <placeholder>`. MySQL + SQLite override to
    /// reject with [`SqlError::OpNotSupportedInDialect`] since
    /// neither has a native range type.
    ///
    /// # Errors
    /// `OpNotSupportedInDialect` when the dialect doesn't support
    /// PG range operators.
    fn write_range_op(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        op: &'static str,
    ) -> Result<(), super::SqlError> {
        sql.push_str(qualified_col);
        sql.push(' ');
        sql.push_str(op);
        sql.push(' ');
        sql.push_str(placeholder);
        Ok(())
    }

    /// Null-safe equality. Postgres: `<col> IS [NOT] DISTINCT FROM <p>`.
    /// `distinct = true` means "not equal under null-safe semantics".
    fn write_null_safe_eq(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholder: &str,
        distinct: bool,
    ) {
        sql.push_str(qualified_col);
        sql.push_str(if distinct {
            " IS DISTINCT FROM "
        } else {
            " IS NOT DISTINCT FROM "
        });
        sql.push_str(placeholder);
    }

    /// JSON containment: `<col> @> <p>::jsonb` (Postgres) /
    /// `JSON_CONTAINS(<col>, <p>)` (MySQL).
    fn write_json_contains(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        sql.push_str(qualified_col);
        sql.push_str(" @> ");
        sql.push_str(placeholder);
        sql.push_str("::jsonb");
    }

    /// Inverse of [`write_json_contains`]: `<col> <@ <p>::jsonb` (Postgres) /
    /// `JSON_CONTAINS(<p>, <col>)` (MySQL — argument order swapped).
    fn write_json_contained_by(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        sql.push_str(qualified_col);
        sql.push_str(" <@ ");
        sql.push_str(placeholder);
        sql.push_str("::jsonb");
    }

    /// Top-level JSON key existence: `<col> ? <p>` (Postgres) /
    /// `JSON_CONTAINS_PATH(<col>, 'one', CONCAT('$.', <p>))` (MySQL).
    fn write_json_has_key(&self, sql: &mut String, qualified_col: &str, placeholder: &str) {
        sql.push_str(qualified_col);
        sql.push_str(" ? ");
        sql.push_str(placeholder);
    }

    /// Multi-key JSON existence. `mode = "one"` matches the
    /// Postgres `?|` "any" operator, `mode = "all"` matches `?&`.
    fn write_json_has_any_keys(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholders: &[String],
    ) {
        write_pg_array_keys(sql, qualified_col, placeholders, " ?| ARRAY[");
    }

    /// `JsonHasAllKeys` companion of [`write_json_has_any_keys`].
    fn write_json_has_all_keys(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholders: &[String],
    ) {
        write_pg_array_keys(sql, qualified_col, placeholders, " ?& ARRAY[");
    }

    /// Append the dialect's spelling of an `ON CONFLICT` / `ON DUPLICATE
    /// KEY UPDATE` clause to `sql`. Default: error — only Postgres
    /// supports the full `ConflictClause` shape today; `MySQL` overrides
    /// to translate `DoNothing` and `DoUpdate { target: vec![], … }`.
    ///
    /// # Errors
    /// [`SqlError::ConflictNotSupportedInDialect`] when this dialect
    /// can't translate the requested shape (e.g. `MySQL` + `DoUpdate`
    /// with a non-empty `target` list — `ON DUPLICATE KEY UPDATE`
    /// has no target-column syntax).
    fn write_conflict_clause(
        &self,
        sql: &mut String,
        conflict: &ConflictClause,
    ) -> Result<(), SqlError> {
        let _ = sql;
        let shape = match conflict {
            ConflictClause::DoNothing => "DO NOTHING",
            ConflictClause::DoUpdate { .. } => "DO UPDATE",
        };
        Err(SqlError::ConflictNotSupportedInDialect {
            shape,
            dialect: self.name(),
        })
    }

    // ====== Advisory locks ======
    //
    // The migration runner serialises concurrent `migrate` /
    // `migrate_to` / `unapply` / `downgrade` calls behind two locks:
    // a session-scoped one held for the whole pending-list apply, and
    // a transaction-scoped one held while creating the ledger table.
    // Each dialect picks the spelling: Postgres uses
    // `pg_advisory_lock` / `pg_advisory_xact_lock`; MySQL would use
    // `GET_LOCK`; SQLite has no native advisory lock but its
    // single-writer model + `BEGIN EXCLUSIVE` emulates the same
    // exclusion (the SQLite impl will return `None` for both
    // session-lock methods and rely on the driver's serialisation).

    /// SQL to acquire a session-scoped advisory lock for `key`.
    /// `key` is the placeholder slot — `placeholder(1)` for Postgres,
    /// for example. Return `None` to skip the lock (SQLite); return
    /// `Some(stmt)` to have the runner execute it on a dedicated
    /// connection. Default returns `None` so dialects that don't
    /// override it just don't take the lock.
    fn acquire_session_lock_sql(&self) -> Option<String> {
        None
    }

    /// SQL to release a session-scoped lock acquired via
    /// [`acquire_session_lock_sql`]. Default `None`. Errors during
    /// release are logged but never propagated — the original
    /// migration error is the one users care about.
    fn release_session_lock_sql(&self) -> Option<String> {
        None
    }

    /// SQL to acquire a transaction-scoped advisory lock for `key`,
    /// auto-released at COMMIT/ROLLBACK. Used by the ledger
    /// bootstrap so two peers don't both pass `CREATE TABLE IF NOT
    /// EXISTS` and then collide on the catalog. Default `None`.
    fn acquire_xact_lock_sql(&self) -> Option<String> {
        None
    }

    /// Inline column-comment fragment to splice into a CREATE TABLE
    /// column definition for the field's `db_comment` attribute (#450).
    /// MySQL returns `" COMMENT '<escaped>'"`; Postgres + SQLite return
    /// `None` because they need a different mechanism (post-hoc
    /// `COMMENT ON COLUMN` for Postgres, no-op for SQLite) — see
    /// [`Self::column_comment_statement`].
    ///
    /// Implementations are responsible for properly escaping single
    /// quotes in the supplied comment.
    fn write_inline_column_comment(&self, _comment: &str) -> Option<String> {
        None
    }

    /// Standalone `COMMENT ON COLUMN "<table>"."<col>" IS '<escaped>'`
    /// statement emitted after CREATE TABLE for dialects that need it.
    /// Postgres returns `Some(_)`; MySQL handles it inline (see
    /// [`Self::write_inline_column_comment`]) and returns `None`;
    /// SQLite returns `None` (no native column comments).
    ///
    /// The migration runner calls this for every field whose
    /// `db_comment.is_some()` after the table is created. Implementations
    /// are responsible for properly escaping single quotes.
    fn column_comment_statement(
        &self,
        _table: &str,
        _column: &str,
        _comment: &str,
    ) -> Option<String> {
        None
    }

    /// Inline table-comment fragment to splice into a CREATE TABLE
    /// trailer for the model's `db_table_comment` attribute. MySQL
    /// returns `" COMMENT='<escaped>'"`; Postgres + SQLite return
    /// `None` because they need a different mechanism (post-hoc
    /// `COMMENT ON TABLE` for Postgres, no-op for SQLite) — see
    /// [`Self::table_comment_statement`]. Implementations are
    /// responsible for properly escaping single quotes.
    fn write_inline_table_comment(&self, _comment: &str) -> Option<String> {
        None
    }

    /// Standalone `COMMENT ON TABLE "<table>" IS '<escaped>'`
    /// statement emitted after CREATE TABLE for dialects that need
    /// it. Postgres returns `Some(_)`; MySQL handles it inline (see
    /// [`Self::write_inline_table_comment`]) and returns `None`;
    /// SQLite returns `None` (no native table comments). Mirrors
    /// the field-level [`Self::column_comment_statement`].
    ///
    /// Implementations are responsible for properly escaping single
    /// quotes in the supplied comment.
    fn table_comment_statement(&self, _table: &str, _comment: &str) -> Option<String> {
        None
    }

    // ====== Compilation (always overridden) ======

    /// Lower a `SelectQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError`] if any filter has a value shape incompatible with
    /// its operator (see the variants for specifics).
    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower an `InsertQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyInsert`] if no columns were supplied, or
    /// [`SqlError::InsertShapeMismatch`] if `columns` and `values` differ in length.
    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower a `BulkInsertQuery` (multi-row INSERT) to one
    /// `CompiledStatement` whose VALUES list has one tuple per input row.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyBulkInsert`] if `rows` is empty (the
    /// caller should short-circuit), [`SqlError::EmptyInsert`] when
    /// `columns` is empty without `returning`, or
    /// [`SqlError::InsertShapeMismatch`] when any row's value count
    /// disagrees with `columns.len()`.
    fn compile_bulk_insert(&self, query: &BulkInsertQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower an `UpdateQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyUpdateSet`] if `set` is empty, or any filter
    /// error from the WHERE clause.
    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower a `DeleteQuery` to a `CompiledStatement` for this dialect.
    ///
    /// # Errors
    /// Returns [`SqlError`] for filter-shape errors in the WHERE clause.
    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower a `CountQuery` to a `SELECT COUNT(*) … WHERE …` statement.
    ///
    /// # Errors
    /// Returns [`SqlError`] for filter-shape errors in the WHERE clause.
    fn compile_count(&self, query: &CountQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower an `AggregateQuery` to a `SELECT … GROUP BY … HAVING …` statement.
    ///
    /// # Errors
    /// Returns [`SqlError`] for filter-shape errors in WHERE/HAVING clauses or
    /// empty `aggregates`.
    fn compile_aggregate(&self, query: &AggregateQuery) -> Result<CompiledStatement, SqlError>;

    /// Lower a `BulkUpdateQuery` to an
    /// `UPDATE t SET col = data.col FROM (VALUES …) AS data(pk, col1, …) WHERE t.pk = data.pk`
    /// statement.
    ///
    /// # Errors
    /// Returns [`SqlError::EmptyBulkInsert`] if `rows` is empty,
    /// [`SqlError::EmptyUpdateSet`] if `update_columns` is empty, or
    /// [`SqlError::MissingPrimaryKey`] if `model` has no PK.
    fn compile_bulk_update(&self, query: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError>;
}
