//! The `Dialect` trait, with one implementation per backend. Every
//! SQL writer dispatches through it.
//!
//! Its methods fall into three groups:
//!
//! * **Compilation** — `compile_select` and friends lower the
//!   dialect-neutral query IR to a [`CompiledStatement`]. Always
//!   overridden.
//! * **DDL primitives** — `quote_ident`, `placeholder`,
//!   `serial_type`, `bool_literal` and the rest, used by
//!   `migrate::ddl`. Most have an ANSI-shaped default; a dialect
//!   overrides only what differs.
//! * **Identity** — `name()`, for error messages and logs.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldType, InsertQuery, Op, SelectQuery, UpdateQuery,
};

use super::{CompiledStatement, SqlError};

/// Write `<col> ?| ARRAY[$1, $2, …]`, shared by both default
/// JSON-key methods.
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

/// Turns the dialect-neutral query IR into a parameterized statement,
/// and supplies the DDL primitives the migration runner needs.
///
/// It is `Send + Sync` because the migration runner holds a
/// `&'static dyn Dialect` across `await` points. Every implementor is
/// a unit struct, so this costs nothing.
pub trait Dialect: Send + Sync {
    /// This dialect's short name: `"postgres"`, `"sqlite"` or
    /// `"mysql"`. For error messages and logs.
    fn name(&self) -> &'static str;

    // ---- DDL primitives, defaulting to the ANSI shape ----

    /// Quote a table or column name. The default is ANSI
    /// double-quotes, which suit PG and SQLite; MySQL uses backticks.
    /// A quote inside the name is doubled, so the result is safe.
    fn quote_ident(&self, name: &str) -> String {
        let escaped = name.replace('"', "\"\"");
        format!("\"{escaped}\"")
    }

    /// Render the placeholder for the `n`-th bind, counting from 1.
    ///
    /// **`n` is advisory.** Only PostgreSQL uses it, as `$n`. SQLite
    /// and MySQL ignore it and emit `?`, so on those backends the
    /// binds must be pushed in the order their placeholders appear
    /// **in the SQL text**. Take `n` from the bind vector's length as
    /// you push, never from a separate counter.
    ///
    /// Two mistakes follow from ignoring that, and both are silent on
    /// PostgreSQL:
    ///
    /// - **Binds out of order.** In `SET ts = {p1} WHERE id IN ({p2})`,
    ///   pushing the timestamp last binds an id into `ts`. The count
    ///   matches, so there is no error, just wrong rows.
    /// - **Reusing a number.** `$1` twice is one bind on PostgreSQL,
    ///   but two `?` needing two binds elsewhere.
    ///
    /// `sql::writers::Sql::push_param` cannot get this wrong, because
    /// it pushes the value and then reads `params.len()`. It is
    /// private to `sql`, so code outside writes the pattern by hand.
    fn placeholder(&self, n: usize) -> String {
        let _ = n;
        "?".to_owned()
    }

    /// The column type for an `Auto<T>` primary key. `field_type` is
    /// `I32` or `I64`; the derive already rejects `Auto<T>` on other
    /// types.
    ///
    /// The default is plain `INTEGER` / `BIGINT`. PG uses `SERIAL`
    /// and `BIGSERIAL`, SQLite `INTEGER PRIMARY KEY AUTOINCREMENT`,
    /// MySQL `BIGINT AUTO_INCREMENT`.
    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "INTEGER",
            _ => "BIGINT",
        }
    }

    /// `true` when [`Self::serial_type`] already includes
    /// `PRIMARY KEY`, as SQLite does: it needs
    /// `INTEGER PRIMARY KEY AUTOINCREMENT` as one phrase. Otherwise
    /// the DDL writer appends `PRIMARY KEY` itself.
    fn serial_type_includes_primary_key(&self) -> bool {
        false
    }

    /// `true` when foreign keys must go inside `CREATE TABLE` instead
    /// of a later `ALTER TABLE … ADD CONSTRAINT`.
    ///
    /// PG and MySQL add them afterwards, so tables that reference
    /// each other resolve across the batch. SQLite has no
    /// `ADD CONSTRAINT`, so its FKs must be in the CREATE.
    fn inline_fks_in_create_table(&self) -> bool {
        false
    }

    /// How many binds the backend takes in one statement. A multi-row
    /// `INSERT` reaches this at `rows × columns`.
    ///
    /// * **Postgres** — 65535, a hard protocol limit.
    /// * **SQLite** — 32766 since 3.32.
    /// * **MySQL** — no fixed cap, only `max_allowed_packet`; 65535
    ///   sits well inside the default.
    fn max_bind_params(&self) -> usize {
        65535
    }

    /// Translate a `DEFAULT` expression from the canonical Postgres
    /// form into this backend's spelling. `ty` is the field type
    /// token, since the right syntax can depend on the column type:
    /// MySQL, for instance, needs `DEFAULT ('{}')` on a JSON column.
    ///
    /// The default passes the expression through. SQLite turns
    /// `now()` into a `strftime` call and drops `::type` casts.
    ///
    /// `max_length` matters only to MySQL, which needs it to tell an
    /// unbounded `String` (a `TEXT` column, which allows no literal
    /// default) from a bounded one (a `VARCHAR(n)`, which does).
    fn translate_default_expr(&self, expr: &str, _ty: &str, _max_length: Option<u32>) -> String {
        expr.to_owned()
    }

    /// The `DEFAULT` expression meaning "the time of the write".
    ///
    /// Hand-written DDL should call this rather than spell it out: on
    /// SQLite it is not a keyword but a `strftime` call in the
    /// canonical format.
    ///
    /// It is only a backstop. Every writer binds its own timestamp.
    fn current_timestamp_default(&self) -> String {
        self.translate_default_expr("now()", "datetime", None)
    }

    /// The full column clause for such a timestamp:
    /// `<type> NOT NULL DEFAULT <now>`.
    fn timestamp_now_column(&self) -> String {
        format!(
            "{} NOT NULL DEFAULT {}",
            self.column_type(FieldType::DateTime, None),
            self.current_timestamp_default()
        )
    }

    /// A boolean literal for `DEFAULT` clauses and inline
    /// comparisons. `TRUE` / `FALSE` by default; SQLite and MySQL use
    /// `1` / `0`, having no boolean type.
    fn bool_literal(&self, b: bool) -> &'static str {
        if b {
            "TRUE"
        } else {
            "FALSE"
        }
    }

    /// `true` if `CREATE INDEX CONCURRENTLY` works. Only Postgres.
    /// Elsewhere a non-atomic migration falls back to a plain
    /// `CREATE INDEX` with a warning.
    fn supports_concurrent_index(&self) -> bool {
        false
    }

    /// `true` if `CREATE INDEX IF NOT EXISTS` is accepted. MySQL has
    /// no such form, so the renderer drops the guard there. The
    /// migration ledger already prevents re-runs.
    fn supports_create_index_if_not_exists(&self) -> bool {
        true
    }

    /// The `USING <method>` clause in
    /// `CREATE INDEX … ON tbl <USING …> (cols)`. Empty for the
    /// default btree, and for SQLite, which has no `USING`.
    ///
    /// An implementation must check the method token and return `""`
    /// for one the backend does not know, so the index is still
    /// created as a btree.
    fn index_method_clause(&self, method: &str) -> String {
        match method {
            "" | "btree" => String::new(),
            other => format!(" USING {other}"),
        }
    }

    /// The "insert or skip" tail for an `INSERT`, so the row is
    /// written the first time and skipped on a re-run.
    ///
    /// `conflict_cols` are **already-quoted** identifiers naming the
    /// unique constraint to match on.
    ///
    /// The default is `ON CONFLICT (…) DO NOTHING`. MySQL uses
    /// `ON DUPLICATE KEY UPDATE <col> = <col>`, a no-op write that
    /// satisfies its requirement for at least one SET expression.
    fn insert_on_conflict_skip(&self, conflict_cols: &[&str]) -> String {
        if conflict_cols.is_empty() {
            return String::new();
        }
        format!("ON CONFLICT ({}) DO NOTHING", conflict_cols.join(", "))
    }

    /// `true` if `INSERT … RETURNING` works: always on Postgres,
    /// from 3.35 on SQLite, never on MySQL. When it is `false`, an
    /// `Auto<T>` insert has to read the id back with a second query.
    fn supports_returning(&self) -> bool {
        false
    }

    /// `true` if `ORDER BY … NULLS FIRST|LAST` is understood. MySQL
    /// is the exception, so the writer sorts on `<col> IS NULL`
    /// first: `DESC` on that term for NULLS FIRST, `ASC` for
    /// NULLS LAST.
    fn supports_nulls_order(&self) -> bool {
        true
    }

    /// The name of the random-number function. `RANDOM` on PG and
    /// SQLite, `RAND` on MySQL.
    fn random_fn(&self) -> &'static str {
        "RANDOM"
    }

    /// The `LIMIT` clause to add when a query has an `OFFSET` but no
    /// `LIMIT`. The writer puts it before the `OFFSET`.
    ///
    /// MySQL's grammar demands a `LIMIT` alongside any `OFFSET`, and
    /// pairs it with the largest `u64`. PG and SQLite accept a bare
    /// `OFFSET`, so they return `None`.
    fn offset_without_limit_clause(&self) -> Option<&'static str> {
        None
    }

    /// Cast a SUM back to BIGINT. PG returns NUMERIC and MySQL
    /// DECIMAL, neither of which the row decoder reads.
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

    /// The type to cast a `NULL` parameter to when the column type is
    /// known. Postgres needs it: an untyped `NULL` bound against an
    /// integer column is rejected, and `$1::INTEGER` says which NULL
    /// is meant. Other dialects return `None` and the writer skips
    /// the cast.
    fn null_cast(&self, ty: FieldType) -> Option<&'static str> {
        let _ = ty;
        None
    }

    /// `ALTER TABLE <table> DROP …` for a named CHECK constraint, or
    /// `None` on a dialect with no such statement, as on SQLite.
    ///
    /// Every dialect must answer for itself. Postgres spells it
    /// `DROP CONSTRAINT IF EXISTS`, which is idempotent. MySQL spells
    /// it `DROP CHECK` and takes no `IF EXISTS`, so it errors when
    /// the constraint is already gone.
    ///
    /// # Panics
    /// The default body panics on purpose, naming the dialect. There
    /// is no safe fallback: handing a new backend the PostgreSQL form
    /// once shipped SQL that MySQL could not parse, and a panic on
    /// the first migration is easier to find than a syntax error from
    /// a live server.
    fn drop_check_constraint_sql(&self, table: &str, name: &str) -> Option<String> {
        let _ = (table, name);
        unimplemented!(
            "Dialect::drop_check_constraint_sql is not implemented for `{}`. \
             Implement it: return the dialect's own `ALTER TABLE … DROP …` \
             spelling, or `None` if it has none. There is deliberately no \
             fallback — inheriting PostgreSQL's form is what shipped invalid \
             SQL to MySQL in #559.",
            self.name()
        )
    }

    /// `ALTER TABLE <table> DROP …` for a named foreign key, or
    /// `None` where there is no such statement.
    ///
    /// Postgres uses `DROP CONSTRAINT IF EXISTS`, MySQL the
    /// non-idempotent `DROP FOREIGN KEY`.
    ///
    /// # Panics
    /// As [`Dialect::drop_check_constraint_sql`].
    fn drop_foreign_key_sql(&self, table: &str, name: &str) -> Option<String> {
        let _ = (table, name);
        unimplemented!(
            "Dialect::drop_foreign_key_sql is not implemented for `{}`. \
             Implement it: return the dialect's own `ALTER TABLE … DROP …` \
             spelling, or `None` if it has none.",
            self.name()
        )
    }

    /// `true` if partial indexes, `CREATE INDEX … WHERE <expr>`, are
    /// supported. MySQL has no equivalent, so the migration writer
    /// drops the WHERE clause there and warns.
    fn supports_partial_index(&self) -> bool {
        true
    }

    /// Does this dialect advertise the given feature token?
    ///
    /// `manage check --deploy` reads every model's
    /// `required_db_features` and warns where this says `false`, so a
    /// project can declare a need like `listen_notify` and hear about
    /// it before deploying rather than at runtime.
    ///
    /// The default covers what all backends share; each dialect adds
    /// its own on top. An unknown token is `false`, so the warning
    /// fires.
    #[must_use]
    fn supports(&self, token: &str) -> bool {
        self.default_supports(token)
    }

    /// The tokens every backend supports. An override of
    /// [`Self::supports`] calls this and adds its own.
    #[must_use]
    fn default_supports(&self, token: &str) -> bool {
        matches!(
            token,
            "window_functions" | "recursive_cte" | "cte" | "json_extract" | "expression_index"
        ) || (token == "partial_index" && self.supports_partial_index())
            || (token == "returning" && self.supports_returning())
    }

    /// The type token for `CAST(<expr> AS <ty>)`. `None` when the
    /// dialect cannot cast to that type at all, as SQLite cannot to
    /// UUID or JSONB; the caller then errors.
    ///
    /// This is neither [`Self::null_cast`], which is PG-only, nor
    /// [`Self::column_type`], which carries lengths for DDL.
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
            FieldType::Array(crate::core::ArrayElem::Text) => "text[]",
            FieldType::Array(crate::core::ArrayElem::Int) => "integer[]",
            FieldType::Array(crate::core::ArrayElem::BigInt) => "bigint[]",
            FieldType::Range(crate::core::RangeElem::Int) => "int4range",
            FieldType::Range(crate::core::RangeElem::BigInt) => "int8range",
            FieldType::Range(crate::core::RangeElem::Numeric) => "numrange",
            FieldType::Range(crate::core::RangeElem::Date) => "daterange",
            FieldType::Range(crate::core::RangeElem::DateTime) => "tstzrange",
            FieldType::HStore => "hstore",
            // `vector(N)` and `geometry(Point, srid)` carry a runtime
            // value in the type, so they have no fixed CAST spelling.
            FieldType::Vector(_) => return None,
            FieldType::Geometry(_) => return None,
        })
    }

    /// The `CREATE TABLE` column type for a field that is not an
    /// `Auto<T>` PK. `max_length` turns a [`FieldType::String`] into
    /// `VARCHAR(N)` instead of unbounded text.
    ///
    /// The default uses Postgres names. MySQL overrides most of them:
    /// it has no `TIMESTAMPTZ`, `JSONB` or `UUID`, and spells
    /// `BOOLEAN` as `TINYINT(1)`.
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
            // An array type has no length cap, so `max_length` is
            // ignored here.
            FieldType::Array(elem) => format!("{}[]", elem.pg_element_type()),
            FieldType::Range(elem) => elem.pg_range_type().to_owned(),
            // Needs the `hstore` extension.
            FieldType::HStore => "hstore".into(),
            // Needs the `vector` extension. 0 dimensions means the
            // column takes any size.
            FieldType::Vector(0) => "vector".into(),
            FieldType::Vector(dims) => format!("vector({dims})"),
            // Needs PostGIS. SRID 0 means no SRID constraint.
            FieldType::Geometry(0) => "geometry(Point)".into(),
            FieldType::Geometry(srid) => format!("geometry(Point,{srid})"),
        }
    }

    /// The column type for a case-insensitive text field: `CITEXT` on
    /// Postgres, `TEXT COLLATE NOCASE` on SQLite, a
    /// `utf8mb4_general_ci` collation on MySQL.
    ///
    /// The default is plain `TEXT`, which leaves case handling to the
    /// query. Use `LOWER(…)` there on a dialect that does not
    /// override this.
    fn ci_text_type(&self, max_length: Option<u32>) -> String {
        let _ = max_length;
        "TEXT".to_owned()
    }

    /// DDL to run once before any case-insensitive column is created,
    /// such as `CREATE EXTENSION IF NOT EXISTS citext` on Postgres.
    /// `None` when nothing is needed.
    fn ci_text_extension_sql(&self) -> Option<&'static str> {
        None
    }

    /// `true` if this dialect can write `op` as SQL. Return `false`
    /// for an operator with no equivalent; the writer then reports
    /// [`SqlError::OperatorNotSupportedInDialect`].
    fn supports_op(&self, op: Op) -> bool {
        let _ = op;
        true
    }

    // ---- per-operator predicate writers ----
    //
    // Each of these gets the column already rendered and quoted, plus
    // a placeholder string, and composes the fragment its parser
    // wants. The defaults are the Postgres shape.

    /// Case-insensitive LIKE: `<col> ILIKE <p>` on Postgres,
    /// `LOWER(<col>) LIKE LOWER(<p>)` elsewhere.
    fn write_ilike(&self, sql: &mut String, qualified_col: &str, placeholder: &str, negated: bool) {
        sql.push_str(qualified_col);
        sql.push_str(if negated { " NOT ILIKE " } else { " ILIKE " });
        sql.push_str(placeholder);
    }

    /// POSIX regex match, for Django's `__regex` and `__iregex`.
    ///
    /// The default is Postgres' `~`, `!~`, `~*` and `!~*`. MySQL and
    /// SQLite use `REGEXP` and `NOT REGEXP`; neither has a
    /// case-insensitive form, so both wrap the column and the
    /// placeholder in `LOWER(…)` for those variants.
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

    /// Trigram similarity, for Django's `__trigram_similar` and
    /// `__trigram_word_similar`. Writes `<col> % <p>`, or
    /// `<col> %> <p>` when `word`. Needs the `pg_trgm` extension.
    ///
    /// # Errors
    /// [`SqlError::OpNotSupportedInDialect`] on MySQL and SQLite,
    /// which have nothing like it.
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

    /// Full-text search, for Django's `__search` lookup. Writes
    /// `to_tsvector(<col>) @@ plainto_tsquery(<p>)`, so the database
    /// picks the config from `default_text_search_config`.
    ///
    /// # Errors
    /// [`SqlError::OpNotSupportedInDialect`] on MySQL and SQLite.
    /// Their full-text search works differently enough that this
    /// shape has no meaning there.
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

    /// Array containment and overlap: `@>`, `<@` and `&&`. `op` is
    /// the operator itself, and the result is
    /// `<col> <op> <placeholder>`.
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

    /// Range operators: `@>`, `<@`, `&&`, `<<`, `>>` and `-|-`. `op`
    /// is the operator itself, and the result is
    /// `<col> <op> <placeholder>`.
    ///
    /// # Errors
    /// [`SqlError::OpNotSupportedInDialect`] on MySQL and SQLite,
    /// which have no range type.
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

    /// The inverse of [`Self::write_json_contains`]:
    /// `<col> <@ <p>::jsonb` on Postgres, `JSON_CONTAINS(<p>, <col>)`
    /// on MySQL, with the arguments the other way round.
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

    /// True when the JSON value has **any** of these keys: Postgres'
    /// `?|` operator.
    fn write_json_has_any_keys(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholders: &[String],
    ) {
        write_pg_array_keys(sql, qualified_col, placeholders, " ?| ARRAY[");
    }

    /// True when the JSON value has **all** of these keys: Postgres'
    /// `?&` operator.
    fn write_json_has_all_keys(
        &self,
        sql: &mut String,
        qualified_col: &str,
        placeholders: &[String],
    ) {
        write_pg_array_keys(sql, qualified_col, placeholders, " ?& ARRAY[");
    }

    /// Append this dialect's `ON CONFLICT` clause. Postgres takes the
    /// full [`ConflictClause`]; MySQL handles `DoNothing` and a
    /// `DoUpdate` with no target columns.
    ///
    /// # Errors
    /// [`SqlError::ConflictNotSupportedInDialect`] when the dialect
    /// cannot express the requested shape. MySQL's
    /// `ON DUPLICATE KEY UPDATE`, for one, has no target-column
    /// syntax.
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

    // ---- Advisory locks ----
    //
    // The migration runner keeps concurrent runs apart with two
    // locks: a session one held while applying the pending list, and
    // a transaction one held while creating the ledger table. SQLite
    // needs neither, since it has a single writer.

    /// SQL that takes a session-scoped advisory lock. The runner
    /// executes it on its own connection. `None` skips the lock.
    fn acquire_session_lock_sql(&self) -> Option<String> {
        None
    }

    /// SQL that releases the lock from
    /// [`Self::acquire_session_lock_sql`]. A failure here is logged,
    /// not returned: the migration's own error matters more.
    fn release_session_lock_sql(&self) -> Option<String> {
        None
    }

    /// SQL that takes a transaction-scoped advisory lock, released at
    /// COMMIT or ROLLBACK. It stops two processes from both passing
    /// `CREATE TABLE IF NOT EXISTS` for the ledger and then colliding.
    fn acquire_xact_lock_sql(&self) -> Option<String> {
        None
    }

    /// The column comment to splice into a `CREATE TABLE` column
    /// definition. Only MySQL writes one here; Postgres uses
    /// [`Self::column_comment_statement`] instead, and SQLite has no
    /// comments at all.
    ///
    /// An implementation must escape single quotes itself.
    fn write_inline_column_comment(&self, _comment: &str) -> Option<String> {
        None
    }

    /// A `COMMENT ON COLUMN` statement to run after `CREATE TABLE`.
    /// Only Postgres needs one; MySQL writes its comment inline in
    /// [`Self::write_inline_column_comment`].
    ///
    /// The runner calls this for every field that has a comment. An
    /// implementation must escape single quotes itself.
    fn column_comment_statement(
        &self,
        _table: &str,
        _column: &str,
        _comment: &str,
    ) -> Option<String> {
        None
    }

    /// The table comment to splice into the `CREATE TABLE` trailer.
    /// [`Self::write_inline_column_comment`] for the whole table.
    fn write_inline_table_comment(&self, _comment: &str) -> Option<String> {
        None
    }

    /// A `COMMENT ON TABLE` statement to run after `CREATE TABLE`.
    /// [`Self::column_comment_statement`] for the whole table.
    fn table_comment_statement(&self, _table: &str, _comment: &str) -> Option<String> {
        None
    }

    // ---- Compilation, always overridden ----

    /// Compile a `SelectQuery` for this dialect.
    ///
    /// # Errors
    /// [`SqlError`] when a filter's value does not suit its operator.
    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError>;

    /// Compile an `InsertQuery` for this dialect.
    ///
    /// # Errors
    /// [`SqlError::EmptyInsert`] with no columns, or
    /// [`SqlError::InsertShapeMismatch`] when `columns` and `values`
    /// are different lengths.
    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError>;

    /// Compile a `BulkInsertQuery` into one statement with a VALUES
    /// tuple per row.
    ///
    /// # Errors
    /// [`SqlError::EmptyBulkInsert`] with no rows,
    /// [`SqlError::EmptyInsert`] when `columns` is empty and nothing
    /// is returned, or [`SqlError::InsertShapeMismatch`] when a row's
    /// length does not match `columns`.
    fn compile_bulk_insert(&self, query: &BulkInsertQuery) -> Result<CompiledStatement, SqlError>;

    /// Compile an `UpdateQuery` for this dialect.
    ///
    /// # Errors
    /// [`SqlError::EmptyUpdateSet`] when `set` is empty, or a filter
    /// error from the WHERE clause.
    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError>;

    /// Compile a `DeleteQuery` for this dialect.
    ///
    /// # Errors
    /// [`SqlError`] for a bad filter in the WHERE clause.
    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError>;

    /// Compile a `CountQuery` into `SELECT COUNT(*) … WHERE …`.
    ///
    /// # Errors
    /// [`SqlError`] for a bad filter in the WHERE clause.
    fn compile_count(&self, query: &CountQuery) -> Result<CompiledStatement, SqlError>;

    /// Compile an `AggregateQuery` into
    /// `SELECT … GROUP BY … HAVING …`.
    ///
    /// # Errors
    /// [`SqlError`] for a bad filter in WHERE or HAVING, or for empty
    /// `aggregates`.
    fn compile_aggregate(&self, query: &AggregateQuery) -> Result<CompiledStatement, SqlError>;

    /// Compile a `BulkUpdateQuery`, updating many rows from one
    /// inline VALUES list joined on the primary key.
    ///
    /// # Errors
    /// [`SqlError::EmptyBulkInsert`] with no rows,
    /// [`SqlError::EmptyUpdateSet`] with no columns, or
    /// [`SqlError::MissingPrimaryKey`] when the model has no PK.
    fn compile_bulk_update(&self, query: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError>;
}

#[cfg(test)]
mod every_dialect_overrides_the_drop_constraint_methods {
    //! Catches a dialect in this crate that forgot to override one of
    //! `Dialect`'s two panicking defaults.
    //!
    //! Those defaults keep a downstream `impl Dialect` compiling, but
    //! they also mean that dropping one of these methods from a
    //! dialect here would still build, and fail instead as a panic
    //! partway through a migration.

    use super::Dialect;

    /// Every dialect answers both, one way or the other. `None` is a
    /// fine answer; inheriting the default is not.
    #[test]
    fn no_dialect_falls_through_to_the_panicking_default() {
        let dialects: Vec<&dyn Dialect> = vec![
            #[cfg(feature = "postgres")]
            &crate::sql::postgres::Postgres,
            #[cfg(feature = "mysql")]
            &crate::sql::mysql::MySql,
            #[cfg(feature = "sqlite")]
            &crate::sql::sqlite::Sqlite,
        ];

        assert!(
            !dialects.is_empty(),
            "no dialect features are on, so this guard checked nothing"
        );

        for d in dialects {
            // The default body panics; any return means the dialect
            // answered for itself.
            let _ = d.drop_check_constraint_sql("t", "c");
            let _ = d.drop_foreign_key_sql("t", "fk");
        }
    }
}
