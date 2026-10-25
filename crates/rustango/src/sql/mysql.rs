//! The MySQL 8.4+ dialect: backtick-quoted identifiers, `?`
//! placeholders, `BIGINT AUTO_INCREMENT` for an `Auto<T>` PK, `1` and
//! `0` for booleans, and `GET_LOCK` for advisory locks. MySQL has no
//! `RETURNING`.
//!
//! An operator with no MySQL translation gets
//! [`SqlError::OperatorNotSupportedInDialect`] from the writers.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldSchema, FieldType, InsertQuery, ModelSchema, SelectQuery, UpdateQuery,
};

use super::server::{AdminRight, Collation, DatabaseName, Login, LoginPassword, ServerAccount};
use super::writers::{
    write_aggregate, write_bulk_insert, write_count, write_delete, write_insert, write_select,
    write_update, Sql,
};
use super::{CompiledStatement, Dialect, SqlError};

/// The insert id a skipped `DoNothing` insert reports. No auto-increment
/// column reaches it: a signed BIGINT stops at 2^63 - 1.
pub(crate) const SKIPPED_INSERT_ID: u64 = u64::MAX;

/// SQL that MySQL and MariaDB spell differently, picked by the server
/// itself: MySQL 8 runs `/*!80000 …*/`, which MariaDB skips, and MariaDB
/// runs `/*M! …*/`, which MySQL skips. Both sides bind the same
/// placeholders in the same order.
#[derive(Clone, Copy)]
pub(crate) struct ByServer<'a> {
    pub mysql: &'a str,
    pub mariadb: &'a str,
}

impl ByServer<'_> {
    pub(crate) fn write(self, sql: &mut String) {
        debug_assert!(!self.mysql.contains("*/") && !self.mariadb.contains("*/"));
        sql.push_str("/*!80000 ");
        sql.push_str(self.mysql);
        sql.push_str(" */ /*M! ");
        sql.push_str(self.mariadb);
        sql.push_str(" */");
    }
}

/// The `MySQL` 8.4+ dialect. Stateless; construct with `MySql`.
#[derive(Debug, Default, Clone, Copy)]
pub struct MySql;

/// The singleton [`MySql`] dialect, which
/// [`crate::sql::Pool::dialect`] hands back for a MySQL pool.
#[cfg(feature = "mysql")]
pub static DIALECT: &MySql = &MySql;

impl MySql {
    /// `user`@`host`, both identifier-quoted.
    fn account(&self, login: &Login) -> String {
        format!(
            "{}@{}",
            self.quote_ident(login.name().as_str()),
            self.quote_ident(login.host())
        )
    }
}

#[cfg(feature = "mysql")]
impl MySql {
    /// The INSERT without `RETURNING`; the id comes from the reply, so an
    /// INSERT naming no column is still a write (#2416).
    pub(crate) fn compile_insert_auto_id(
        &self,
        query: &InsertQuery,
    ) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::with_capacity(self, query.values.len());
        super::writers::write_insert_rows(&mut b, query)?;
        b.finish()
    }
}

impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn binds_pg_only_values(&self) -> bool {
        false
    }

    fn default_values_clause(&self) -> &'static str {
        " () VALUES ()"
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

    /// A materialized derived table also lifts error 1093, so the
    /// subquery of an UPDATE/DELETE may read the target table.
    fn in_subquery_limit_needs_derived_table(&self) -> bool {
        true
    }

    /// MySQL quotes with backticks, and an embedded backtick is
    /// doubled, so any name comes out valid.
    fn quote_ident(&self, name: &str) -> String {
        super::dialect::quote_with(name, '`')
    }

    /// MySQL reads `\` as an escape inside a string literal under the
    /// default `sql_mode`, so it is doubled along with `'` (#2232).
    /// On a `NO_BACKSLASH_ESCAPES` server the `\` is stored doubled.
    fn quote_literal(&self, text: &str) -> String {
        format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
    }

    /// utf8mb4: older MariaDB defaults to latin1. Without `collation` the
    /// server picks utf8mb4's default, which ignores `collation_server`.
    fn create_database_sql(
        &self,
        db: &DatabaseName,
        collation: Option<&Collation>,
    ) -> Option<String> {
        let collate = collation.map_or_else(String::new, |c| format!(" COLLATE {}", c.as_str()));
        Some(format!(
            "CREATE DATABASE {} CHARACTER SET utf8mb4{collate}",
            self.quote_ident(db.as_str())
        ))
    }

    fn drop_database_sql(&self, db: &DatabaseName) -> Option<String> {
        Some(format!(
            "DROP DATABASE IF EXISTS {}",
            self.quote_ident(db.as_str())
        ))
    }

    /// The account is identifier-quoted, so only the password is a
    /// literal; [`Self::admin_session_sql`] pins its escaping.
    fn create_login_sql(&self, login: &Login, password: &LoginPassword) -> Option<String> {
        Some(format!(
            "CREATE USER {} IDENTIFIED BY {}",
            self.account(login),
            self.quote_literal(password.expose())
        ))
    }

    fn drop_login_sql(&self, login: &Login) -> Option<String> {
        Some(format!("DROP USER IF EXISTS {}", self.account(login)))
    }

    fn collation_exists_sql(&self, collation: &Collation) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: "SELECT count(*) FROM information_schema.COLLATIONS WHERE COLLATION_NAME = ?"
                .to_owned(),
            params: vec![crate::core::SqlValue::String(collation.as_str().to_owned())],
        })
    }

    /// `retain_current` needs MySQL 8.0.14+; MariaDB has no dual passwords.
    fn alter_login_password_sql(
        &self,
        login: &Login,
        password: &LoginPassword,
        retain_current: bool,
    ) -> Option<String> {
        let retain = if retain_current {
            " RETAIN CURRENT PASSWORD"
        } else {
            ""
        };
        Some(format!(
            "ALTER USER {} IDENTIFIED BY {}{retain}",
            self.account(login),
            self.quote_literal(password.expose())
        ))
    }

    fn discard_old_password_sql(&self, login: &Login) -> Option<String> {
        Some(format!(
            "ALTER USER {} DISCARD OLD PASSWORD",
            self.account(login)
        ))
    }

    fn database_exists_sql(&self, db: &DatabaseName) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: "SELECT count(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?"
                .to_owned(),
            params: vec![crate::core::SqlValue::String(db.as_str().to_owned())],
        })
    }

    /// Needs SELECT on `mysql.user`; callers treat a refusal as unknown.
    fn login_exists_sql(&self, login: &Login) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: "SELECT count(*) FROM mysql.user WHERE User = ? AND Host = ?".to_owned(),
            params: vec![
                crate::core::SqlValue::String(login.name().as_str().to_owned()),
                crate::core::SqlValue::String(login.host().to_owned()),
            ],
        })
    }

    /// `DatabaseName` holds no `_` or `%`, the wildcards of `db.*`.
    fn grant_database_sql(&self, db: &DatabaseName, login: &Login) -> Vec<String> {
        vec![format!(
            "GRANT ALL PRIVILEGES ON {}.* TO {}",
            self.quote_ident(db.as_str()),
            self.account(login)
        )]
    }

    fn server_version_sql(&self) -> Option<&'static str> {
        Some("SELECT VERSION()")
    }

    /// Rows of (`PRIVILEGE_TYPE`, `IS_GRANTABLE`): the current account's
    /// global privileges. MariaDB has the same view.
    fn login_privileges_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.USER_PRIVILEGES \
             WHERE GRANTEE = CONCAT('''', SUBSTRING_INDEX(CURRENT_USER(), '@', 1), \
             '''@''', SUBSTRING_INDEX(CURRENT_USER(), '@', -1), '''')",
        )
    }

    /// MySQL folds active roles' rights into the user's line; MariaDB
    /// lists each role, nested ones too.
    fn login_grants_sql(&self) -> Option<&'static str> {
        Some("SHOW GRANTS")
    }

    /// `user@host`; the host follows the last `@`.
    fn current_account_sql(&self) -> Option<&'static str> {
        Some("SELECT CAST(CURRENT_USER() AS CHAR)")
    }

    fn grant_admin_rights_sql(
        &self,
        account: &ServerAccount,
        _database: Option<&str>,
        rights: &[AdminRight],
    ) -> Vec<String> {
        let to = format!(
            "{}@{}",
            self.quote_literal(&account.user),
            self.quote_literal(account.host.as_deref().unwrap_or("%"))
        );
        let mut privs = Vec::new();
        if rights.contains(&AdminRight::CreateDatabase) {
            privs.push("CREATE, DROP");
        }
        if rights.contains(&AdminRight::CreateLogin) {
            privs.push("CREATE USER");
        }
        let mut out = Vec::new();
        if !privs.is_empty() {
            out.push(format!("GRANT {} ON *.* TO {to}", privs.join(", ")));
        }
        // GRANT ALL ON db.* needs every database privilege grantable.
        if rights.contains(&AdminRight::Grant) {
            out.push(format!("GRANT ALL PRIVILEGES ON *.* TO {to} WITH GRANT OPTION"));
        }
        out
    }

    /// sqlx appends to the server's `sql_mode`, so a server-wide
    /// `NO_BACKSLASH_ESCAPES` would make [`Self::quote_literal`] wrong.
    fn admin_session_sql(&self) -> Option<&'static str> {
        Some(
            "SET SESSION sql_mode = TRIM(BOTH ',' FROM REPLACE(\
             CONCAT(',', @@SESSION.sql_mode, ','), ',NO_BACKSLASH_ESCAPES,', ','))",
        )
    }

    /// MySQL writes a column comment inline, after the rest of the
    /// column definition.
    fn write_inline_column_comment(&self, comment: &str) -> Option<String> {
        Some(format!(" COMMENT {}", self.quote_literal(comment)))
    }

    /// MySQL writes a table comment as a `COMMENT='…'` trailer after
    /// the closing paren.
    fn write_inline_table_comment(&self, comment: &str) -> Option<String> {
        Some(format!(" COMMENT={}", self.quote_literal(comment)))
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
    /// `UNSIGNED`, strings to `CHAR`, and timestamps to `DATETIME(6)`
    /// (bare `DATETIME` / `TIME` drop the microseconds).
    ///
    /// UUID has no CAST target, so it returns `None`; cast a
    /// string-shaped value to `CHAR` yourself.
    fn cast_type(&self, ty: FieldType) -> Option<&'static str> {
        Some(match ty {
            FieldType::I16 | FieldType::I32 | FieldType::I64 => "SIGNED",
            FieldType::F32 => "FLOAT",
            FieldType::F64 => "DOUBLE",
            FieldType::Bool => "UNSIGNED",
            FieldType::String => "CHAR",
            FieldType::DateTime => "DATETIME(6)",
            FieldType::Date => "DATE",
            FieldType::Time => "TIME(6)",
            FieldType::Decimal => "DECIMAL(65, 28)",
            FieldType::Binary => "BINARY",
            FieldType::Json => "JSON",
            // UUID has no target, and the array, range and other
            // Postgres-only types have nothing to cast to.
            FieldType::Uuid
            | FieldType::Array(_)
            | FieldType::Range(_)
            | FieldType::HStore
            | FieldType::Vector(_)
            | FieldType::Geometry(_) => return None,
        })
    }

    /// InnoDB caps a key at 3072 bytes (1071); utf8mb4 counts 4 per char.
    fn index_key_size(&self, cols: &[(FieldType, Option<u32>)]) -> Option<(u32, u32)> {
        let bytes = cols
            .iter()
            .map(|(ty, len)| match (ty, len) {
                (FieldType::String, Some(n)) => n.saturating_mul(4),
                (FieldType::Uuid, _) => 36 * 4,
                (FieldType::Decimal, _) => 30,
                // LONGTEXT / LONGBLOB / JSON take no key without a prefix (ERROR 1170 / 3152).
                (FieldType::String, None) | (FieldType::Binary | FieldType::Json, _) => u32::MAX,
                // Every other key-able type here is at most 8 bytes.
                _ => 8,
            })
            .fold(0u32, u32::saturating_add);
        Some((bytes, 3072))
    }

    /// MySQL's column types differ from the Postgres ones in
    /// several ways:
    /// - no `BOOLEAN`; it is an alias for `TINYINT(1)`
    /// - no `TIMESTAMPTZ`; `DATETIME(6)` holds microseconds but no
    ///   timezone, and sqlx binds a `DateTime<Utc>` into it correctly
    /// - no `JSONB`; `JSON` validates on write and stores binary
    /// - no `UUID`; `CHAR(36)` is the usual form
    /// - `VARCHAR` needs a length, so an unbounded `String` becomes
    ///   `LONGTEXT`; `TEXT` caps at 64 KiB where PG/SQLite have no cap
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
                None => "LONGTEXT".into(),
            },
            FieldType::DateTime => "DATETIME(6)".into(),
            FieldType::Date => "DATE".into(),
            FieldType::Uuid => "CHAR(36)".into(),
            FieldType::Json => "JSON".into(),
            // A bare `DECIMAL` is `(10, 0)`, which has no fraction.
            // `rust_decimal` holds 29 integer and 28 fractional digits;
            // `(65, 28)` stores all of them.
            FieldType::Decimal => "DECIMAL(65, 28)".into(),
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

    // Case-insensitive only, as PG citext. The server picks: MySQL 8 runs
    // `/*!80000`, MariaDB 10.10+ only `/*M!101000`; older ones keep the
    // charset default `general_ci`, which also ignores accents and spaces.
    fn ci_text_type(&self, max_length: Option<u32>) -> String {
        const CI: &str = "CHARACTER SET utf8mb4 /*!80000 COLLATE utf8mb4_0900_as_ci */ \
                          /*M!101000 COLLATE utf8mb4_uca1400_nopad_as_ci */";
        match max_length {
            Some(n) => format!("VARCHAR({n}) {CI}"),
            None => format!("LONGTEXT {CI}"),
        }
    }

    // Columns inherit it, and a `_ci` one makes `=` and `unique` ignore
    // case and accents, unlike PG and SQLite (#1742).
    fn default_collation_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT CAST(DEFAULT_COLLATION_NAME AS CHAR) FROM information_schema.SCHEMATA \
             WHERE SCHEMA_NAME = DATABASE()",
        )
    }

    // CONVERT_TZ returns NULL for a named zone when the server's time
    // zone tables are not loaded.
    fn named_time_zone_sql(&self) -> Option<&'static str> {
        Some("SELECT CAST(CONVERT_TZ('2000-01-01 00:00:00', '+00:00', 'UTC') AS CHAR)")
    }

    /// Translate Postgres-native `DEFAULT` expressions to MySQL
    /// spelling.
    ///
    /// - `now()` becomes `CURRENT_TIMESTAMP(6)`. MySQL wants the
    ///   default's precision to match the column, and a `DateTime`
    ///   column is `DATETIME(6)`; without the `(6)` it rejects the
    ///   default outright.
    /// - `gen_random_uuid()` becomes the 8.0.13+ expression `(UUID())`.
    /// - `'<lit>'::<type>` becomes `'<lit>'`.
    /// - A JSON, TEXT or BLOB column takes no literal default, only
    ///   MySQL 8.0.13+'s `DEFAULT (<expr>)` form, so those get
    ///   wrapped in parens. `max_length` is what tells an unbounded
    ///   `String`, which is LONGTEXT, from a `VARCHAR(n)`, which keeps
    ///   its literal default.
    /// - Everything else passes through.
    fn translate_default_expr(&self, expr: &str, ty: &str, max_length: Option<u32>) -> String {
        let mut out = expr.trim().to_owned();
        match out.as_str() {
            "now()" | "NOW()" | "current_timestamp" | "CURRENT_TIMESTAMP" => {
                out = "CURRENT_TIMESTAMP(6)".to_owned();
            }
            e if super::dialect::is_uuid_expr(e) => return "(UUID())".to_owned(),
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

    /// InnoDB parses `SET DEFAULT` and refuses the parent delete (1451).
    fn supports_on_delete_set_default(&self) -> bool {
        false
    }

    /// MySQL spells this `DROP CHECK`, MariaDB `DROP CONSTRAINT`, and
    /// neither takes `IF EXISTS` there.
    ///
    /// **So this is not idempotent.** Dropping a constraint that is
    /// already gone is an error, where the Postgres form does
    /// nothing.
    fn drop_check_constraint_sql(&self, table: &str, name: &str) -> Option<String> {
        let mut sql = format!("ALTER TABLE {} DROP ", self.quote_ident(table));
        ByServer {
            mysql: "CHECK",
            mariadb: "CONSTRAINT",
        }
        .write(&mut sql);
        sql.push(' ');
        sql.push_str(&self.quote_ident(name));
        Some(sql)
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

    /// No deferred FKs: the hint drops checks for one statement only, so
    /// no session state leaks back into the pool (a `SET` would).
    /// `foreign_key_checks` is per session or per statement, never per transaction.
    fn defer_foreign_keys_sql(&self) -> Option<&'static str> {
        None
    }

    fn clear_tables_sql(&self, tables: &[&str]) -> Vec<String> {
        tables
            .iter()
            .map(|t| {
                format!(
                    "DELETE /*+ SET_VAR(foreign_key_checks=0) */ FROM {}",
                    self.quote_ident(t)
                )
            })
            .collect()
    }

    /// `CAST AS CHAR`: information_schema columns decode as binary otherwise.
    fn foreign_key_names_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT CAST(k.CONSTRAINT_NAME AS CHAR) FROM information_schema.KEY_COLUMN_USAGE k \
             WHERE k.TABLE_SCHEMA = DATABASE() AND k.TABLE_NAME = ? AND k.COLUMN_NAME = ? \
             AND k.REFERENCED_TABLE_NAME IS NOT NULL AND NOT EXISTS (\
             SELECT 1 FROM information_schema.KEY_COLUMN_USAGE o \
             WHERE o.TABLE_SCHEMA = k.TABLE_SCHEMA AND o.TABLE_NAME = k.TABLE_NAME \
             AND o.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND o.ORDINAL_POSITION > 1)",
        )
    }

    fn referencing_foreign_keys_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT CAST(k.TABLE_NAME AS CHAR), CAST(k.CONSTRAINT_NAME AS CHAR) \
             FROM information_schema.KEY_COLUMN_USAGE k \
             WHERE k.TABLE_SCHEMA = DATABASE() AND k.REFERENCED_TABLE_SCHEMA = DATABASE() \
             AND k.REFERENCED_TABLE_NAME = ? AND k.REFERENCED_COLUMN_NAME = ? AND NOT EXISTS (\
             SELECT 1 FROM information_schema.KEY_COLUMN_USAGE o \
             WHERE o.TABLE_SCHEMA = k.TABLE_SCHEMA AND o.TABLE_NAME = k.TABLE_NAME \
             AND o.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND o.ORDINAL_POSITION > 1)",
        )
    }

    fn sole_leading_column_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT CAST(s.COLUMN_NAME AS CHAR) FROM information_schema.STATISTICS s \
             WHERE s.TABLE_SCHEMA = DATABASE() AND s.TABLE_NAME = ? AND s.INDEX_NAME = ? \
             AND s.SEQ_IN_INDEX = 1 AND NOT EXISTS (\
             SELECT 1 FROM information_schema.STATISTICS o \
             WHERE o.TABLE_SCHEMA = s.TABLE_SCHEMA AND o.TABLE_NAME = s.TABLE_NAME \
             AND o.COLUMN_NAME = s.COLUMN_NAME AND o.SEQ_IN_INDEX = 1 \
             AND o.INDEX_NAME <> s.INDEX_NAME \
             AND o.SUB_PART IS NULL AND o.INDEX_TYPE = 'BTREE')",
        )
    }

    /// Each FK column must sit at its position in the index, unprefixed.
    fn composite_fks_needing_index_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT CAST(k.CONSTRAINT_NAME AS CHAR) FROM information_schema.KEY_COLUMN_USAGE k \
             WHERE k.TABLE_SCHEMA = DATABASE() AND k.TABLE_NAME = ? \
             AND k.REFERENCED_TABLE_NAME IS NOT NULL AND k.ORDINAL_POSITION = 2 \
             AND NOT EXISTS (SELECT 1 FROM information_schema.KEY_COLUMN_USAGE f \
             WHERE f.TABLE_SCHEMA = k.TABLE_SCHEMA AND f.TABLE_NAME = k.TABLE_NAME \
             AND f.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND NOT EXISTS (\
             SELECT 1 FROM information_schema.STATISTICS s \
             WHERE s.TABLE_SCHEMA = f.TABLE_SCHEMA AND s.TABLE_NAME = f.TABLE_NAME \
             AND s.INDEX_NAME = ? AND s.SEQ_IN_INDEX = f.ORDINAL_POSITION \
             AND s.COLUMN_NAME = f.COLUMN_NAME AND s.SUB_PART IS NULL)) \
             AND NOT EXISTS (SELECT 1 FROM information_schema.STATISTICS j \
             WHERE j.TABLE_SCHEMA = k.TABLE_SCHEMA AND j.TABLE_NAME = k.TABLE_NAME \
             AND j.INDEX_NAME <> ? AND j.SEQ_IN_INDEX = 1 \
             AND NOT EXISTS (SELECT 1 FROM information_schema.KEY_COLUMN_USAGE f \
             WHERE f.TABLE_SCHEMA = k.TABLE_SCHEMA AND f.TABLE_NAME = k.TABLE_NAME \
             AND f.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND NOT EXISTS (\
             SELECT 1 FROM information_schema.STATISTICS s \
             WHERE s.TABLE_SCHEMA = j.TABLE_SCHEMA AND s.TABLE_NAME = j.TABLE_NAME \
             AND s.INDEX_NAME = j.INDEX_NAME AND s.SEQ_IN_INDEX = f.ORDINAL_POSITION \
             AND s.COLUMN_NAME = f.COLUMN_NAME AND s.SUB_PART IS NULL \
             AND s.INDEX_TYPE = 'BTREE')))",
        )
    }

    fn modifies_whole_column(&self) -> bool {
        true
    }

    // sqlx decodes any MySQL integer as `i64`, so the junction need not widen.
    fn fk_needs_same_int_width(&self) -> bool {
        true
    }

    /// `PRIMARY` and multi-column indexes are left out.
    fn unique_index_names_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT CAST(s.INDEX_NAME AS CHAR) FROM information_schema.STATISTICS s \
             WHERE s.TABLE_SCHEMA = DATABASE() AND s.TABLE_NAME = ? AND s.COLUMN_NAME = ? \
             AND s.NON_UNIQUE = 0 AND s.INDEX_NAME <> 'PRIMARY' AND NOT EXISTS (\
             SELECT 1 FROM information_schema.STATISTICS o \
             WHERE o.TABLE_SCHEMA = s.TABLE_SCHEMA AND o.TABLE_NAME = s.TABLE_NAME \
             AND o.INDEX_NAME = s.INDEX_NAME AND o.SEQ_IN_INDEX > 1)",
        )
    }

    fn drop_unique_index_sql(&self, table: &str, name: &str) -> Option<String> {
        Some(format!(
            "ALTER TABLE {} DROP INDEX {}",
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

    /// Bound as text, a JSON value never equals a JSON column. MariaDB has
    /// no JSON cast and skips `/*!80000`; its JSON is text anyway.
    fn write_json_param(&self, sql: &mut String, placeholder: &str) {
        sql.push_str("/*!80000 CAST(*/");
        sql.push_str(placeholder);
        sql.push_str("/*!80000 AS JSON)*/");
    }

    /// MariaDB's JSON is text, so `=` would compare key order and `1` vs `1.0`.
    fn write_json_eq(&self, sql: &mut String, lhs: &str, placeholder: &str, ne: bool) -> bool {
        // A comment in `lhs` would end the per-server one.
        if lhs.contains("*/") {
            return false;
        }
        let (op, want) = if ne { ("<>", 0) } else { ("=", 1) };
        // Bare in a WHERE, MariaDB 11 takes a NULL `JSON_EQUALS` as true.
        ByServer {
            mysql: &format!("{lhs} {op} CAST({placeholder} AS JSON)"),
            mariadb: &format!("JSON_EQUALS({lhs}, {placeholder}) = {want}"),
        }
        .write(sql);
        true
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
        // `JSON_CONTAINS_PATH(col, 'one', '$."key"')` is MySQL's
        // top-level key check.
        write_my_json_has_keys(sql, qualified_col, &[placeholder], "one");
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
    /// list; it fires on any unique violation, so a `DoUpdate` is
    /// refused unless its target is the only unique key a row can hit:
    ///
    /// ```sql
    /// INSERT INTO `t` (a, b) VALUES (?, ?)
    /// ON DUPLICATE KEY UPDATE `a` = VALUES(`a`), `b` = VALUES(`b`)
    /// ```
    ///
    /// `DoNothing` becomes a self-assignment, which skips the
    /// duplicate. `INSERT IGNORE` would do that too, but it also hides
    /// every other error. The assignment also sets
    /// [`SKIPPED_INSERT_ID`]: sqlx sets `CLIENT_FOUND_ROWS`, so a skip
    /// reports 1 affected row, the same as an insert.
    ///
    /// On an auto-increment PK, `DoUpdate`'s first assignment also calls
    /// `LAST_INSERT_ID(pk)`, so it names the updated row, not a stale one.
    fn write_conflict_clause(
        &self,
        sql: &mut String,
        model: &ModelSchema,
        caller_set: &[&str],
        conflict: &ConflictClause,
    ) -> Result<(), SqlError> {
        match conflict {
            ConflictClause::DoNothing => {
                sql.push_str(" ON DUPLICATE KEY UPDATE ");
                write_my_skip_marker(sql, model, &SKIPPED_INSERT_ID.to_string())?;
            }
            ConflictClause::DoUpdate {
                target,
                update_columns,
            } => {
                if update_columns.is_empty() {
                    return Err(SqlError::EmptyUpdateSet);
                }
                // The clause fires on every unique key, so another key
                // that can clash would update the wrong row.
                if !my_target_is_only_unique_key(model, caller_set, target) {
                    return Err(SqlError::ConflictNotSupportedInDialect {
                        shape: "DO UPDATE on a target that is not the only unique key",
                        dialect: "mysql",
                    });
                }
                sql.push_str(" ON DUPLICATE KEY UPDATE ");
                // The id rides inside the first assignment: assigning the
                // auto-increment PK fails with 1869 in a batch (#2200).
                let mut id = model.primary_key().filter(|f| f.is_serial()).map(|pk| {
                    let mut id = String::new();
                    write_my_ident(&mut id, pk.column);
                    id
                });
                for (i, col) in update_columns.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    let mut value = String::from("VALUES(");
                    write_my_ident(&mut value, col);
                    value.push(')');
                    write_my_ident(sql, col);
                    sql.push_str(" = ");
                    match id.take() {
                        Some(id) => write_my_set_insert_id(sql, &id, &value),
                        None => sql.push_str(&value),
                    }
                }
            }
        }
        Ok(())
    }

    // ---- advisory locks ----

    // GET_LOCK names are server-wide: suffix the database so tenants don't share one (#1991).
    fn acquire_session_lock_sql(&self) -> Option<String> {
        Some(format!(
            "SELECT GET_LOCK({}, 0)",
            my_db_lock_name(self.placeholder(1))
        ))
    }

    fn release_session_lock_sql(&self) -> Option<String> {
        Some(format!(
            "SELECT RELEASE_LOCK({})",
            my_db_lock_name(self.placeholder(1))
        ))
    }

    // MySQL has no transaction-scoped advisory lock.

    // ---- compilation ----

    fn compile_select(&self, query: &SelectQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_select(&mut b, query)?;
        b.finish()
    }

    fn compile_count(&self, query: &CountQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_count(&mut b, query)?;
        b.finish()
    }

    fn compile_aggregate(&self, query: &AggregateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_aggregate(&mut b, query)?;
        b.finish()
    }

    fn compile_insert(&self, query: &InsertQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::with_capacity(self, query.values.len());
        write_insert(&mut b, query)?;
        b.finish()
    }

    fn compile_bulk_insert(&self, query: &BulkInsertQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::with_capacity(self, query.columns.len() * query.rows.len());
        write_bulk_insert(&mut b, query)?;
        b.finish()
    }

    fn compile_update(&self, query: &UpdateQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_update(&mut b, query)?;
        b.finish()
    }

    fn compile_delete(&self, query: &DeleteQuery) -> Result<CompiledStatement, SqlError> {
        let mut b = Sql::new(self);
        write_delete(&mut b, query)?;
        b.finish()
    }

    fn compile_bulk_update(&self, query: &BulkUpdateQuery) -> Result<CompiledStatement, SqlError> {
        // Needs MySQL 8.0.19+, for the `VALUES ROW(…)` table
        // constructor. On anything older, update row by row.
        let mut b = Sql::new(self);
        write_mysql_bulk_update(&mut b, query)?;
        b.finish()
    }
}

/// Bound prefix + SHA1 of the current database: 57 chars, under MySQL's 64-char lock-name cap.
fn my_db_lock_name(prefix: impl std::fmt::Display) -> String {
    format!("CONCAT({prefix}, SHA1(COALESCE(DATABASE(), '')))")
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

/// True when every unique key a new row can clash on is `target`. A key
/// with an `Auto` column the caller did not set holds a generated value,
/// so it never clashes; a caller-set one can.
fn my_target_is_only_unique_key(model: &ModelSchema, caller_set: &[&str], target: &[&str]) -> bool {
    let fits = |cols: &[&str]| {
        let generated = cols.iter().any(|c| {
            model
                .field_by_column(c)
                .is_some_and(|f| f.auto && !caller_set.contains(c))
        });
        generated || (cols.len() == target.len() && cols.iter().all(|c| target.contains(c)))
    };
    let pk: Vec<&str> = model
        .fields
        .iter()
        .filter(|f| f.primary_key)
        .map(|f| f.column)
        .collect();
    (pk.is_empty() || fits(&pk))
        && model
            .fields
            .iter()
            .filter(|f| f.unique && !f.primary_key)
            .all(|f| fits(&[f.column]))
        && model
            .indexes
            .iter()
            .filter(|i| i.unique)
            .all(|i| fits(i.columns))
}

/// `<col> = IF(LAST_INSERT_ID(<id>), <col>, <col>)`: a no-op write that
/// sets the insert id and leaves `ON UPDATE CURRENT_TIMESTAMP` alone.
/// `<col>` avoids the auto-increment column, whose assignment fails with
/// 1869 once two rows of one batch collide (#2200); a table whose other
/// columns are all generated falls back to it and can still hit 1869.
fn write_my_skip_marker(sql: &mut String, model: &ModelSchema, id: &str) -> Result<(), SqlError> {
    let plain = |f: &&FieldSchema| !f.is_serial() && f.generated_as.is_none();
    let pivot = model
        .primary_key()
        .filter(plain)
        .or_else(|| model.fields.iter().find(plain))
        .or_else(|| model.primary_key())
        .ok_or(SqlError::MissingPrimaryKey)?
        .column;
    let mut col = String::new();
    write_my_ident(&mut col, pivot);
    sql.push_str(&col);
    sql.push_str(" = ");
    write_my_set_insert_id(sql, id, &col);
    Ok(())
}

/// `IF(LAST_INSERT_ID(<id>), <value>, <value>)`: `<value>`, with the
/// session insert id set to `<id>` on the way.
fn write_my_set_insert_id(sql: &mut String, id: &str, value: &str) {
    sql.push_str("IF(LAST_INSERT_ID(");
    sql.push_str(id);
    sql.push_str("), ");
    sql.push_str(value);
    sql.push_str(", ");
    sql.push_str(value);
    sql.push(')');
}

/// Shared body of [`MySql::write_json_has_any_keys`] /
/// [`MySql::write_json_has_all_keys`]. `mode` is `"one"` for "any key
/// matches" or `"all"` for "every key matches".
fn write_my_json_has_keys<P: AsRef<str>>(
    sql: &mut String,
    qualified_col: &str,
    placeholders: &[P],
    mode: &'static str,
) {
    sql.push_str("JSON_CONTAINS_PATH(");
    sql.push_str(qualified_col);
    sql.push_str(", '");
    sql.push_str(mode);
    sql.push('\'');
    // `JSON_QUOTE` makes the bound key one literal member, as PG's `?`
    // reads it; bare, `a.b` or `*` is a path and `2fa` a syntax error.
    for p in placeholders {
        sql.push_str(", CONCAT('$.', JSON_QUOTE(");
        sql.push_str(p.as_ref());
        sql.push_str("))");
    }
    sql.push(')');
}

/// MySQL's bulk UPDATE: `UPDATE t INNER JOIN (VALUES ROW(…), …) AS
/// d(pk, c1, …) ON t.pk = d.pk SET t.c1 = d.c1`. MariaDB has no `ROW(…)`
/// and gets `(SELECT ? AS pk, … UNION ALL SELECT …) AS d` instead. It lives
/// here rather than in `writers` because the syntax is MySQL's own.
fn write_mysql_bulk_update(
    b: &mut crate::sql::writers::Sql<'_>,
    query: &crate::core::BulkUpdateQuery,
) -> Result<(), SqlError> {
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
    let columns: Vec<String> = std::iter::once(pk_field.column)
        .chain(query.update_columns.iter().copied())
        .map(|c| b.d.quote_ident(c))
        .collect();

    b.sql.push_str("UPDATE ");
    b.write_ident(query.model.table);
    b.sql.push_str(" INNER JOIN (");
    // Both spellings bind the rows in the same order.
    let (mut my, mut maria) = (String::from("VALUES "), String::new());
    for (r, row) in query.rows.iter().enumerate() {
        my.push_str(if r == 0 { "ROW(" } else { ", ROW(" });
        maria.push_str(if r == 0 { "SELECT " } else { " UNION ALL SELECT " });
        for (i, val) in row.iter().enumerate() {
            b.params.push(val.clone());
            let p = b.d.placeholder(b.params.len());
            let sep = if i > 0 { ", " } else { "" };
            my.push_str(sep);
            my.push_str(&p);
            maria.push_str(sep);
            maria.push_str(&p);
            if let (0, Some(c)) = (r, columns.get(i)) {
                maria.push_str(" AS ");
                maria.push_str(c);
            }
        }
        my.push(')');
    }
    my.push_str(") AS __data(");
    my.push_str(&columns.join(", "));
    my.push(')');
    maria.push_str(") AS __data");
    ByServer {
        mysql: &my,
        mariadb: &maria,
    }
    .write(&mut b.sql);
    b.sql.push_str(" ON ");
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
    fn quote_literal_escapes_backslash_and_quote() {
        assert_eq!(MySql.quote_literal(r"a\'b"), r"'a\\''b'");
        assert_eq!(crate::sql::Postgres.quote_literal(r"a\'b"), r"'a\''b'");
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
    fn unbounded_string_is_longtext() {
        // #1708: TEXT caps at 64 KiB; PG and SQLite text has no cap.
        assert_eq!(MySql.column_type(FieldType::String, None), "LONGTEXT");
        assert_eq!(
            MySql.column_type(FieldType::String, Some(64)),
            "VARCHAR(64)"
        );
        assert_eq!(
            MySql.ci_text_type(None),
            "LONGTEXT CHARACTER SET utf8mb4 /*!80000 COLLATE utf8mb4_0900_as_ci */ \
             /*M!101000 COLLATE utf8mb4_uca1400_nopad_as_ci */"
        );
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
        // one of them, since it renders as LONGTEXT.
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
        assert!(acq.contains("DATABASE()"), "{acq}");
        let rel = MySql.release_session_lock_sql().unwrap();
        assert!(rel.contains("RELEASE_LOCK"));
        assert!(rel.contains("DATABASE()"), "{rel}");
    }

    #[test]
    fn xact_lock_is_none() {
        assert!(MySql.acquire_xact_lock_sql().is_none());
    }

    #[test]
    fn conflict_do_nothing_assigns_the_pk_and_marks_the_skip() {
        let model = with_pk(
            empty_model_with("t", &[("code", FieldType::String), ("n", FieldType::I64)]),
            "code",
            false,
        );
        let mut sql = String::new();
        MySql
            .write_conflict_clause(&mut sql, model, &[], &ConflictClause::DoNothing)
            .unwrap();
        assert_eq!(
            sql,
            " ON DUPLICATE KEY UPDATE `code` = IF(LAST_INSERT_ID(18446744073709551615), `code`, `code`)"
        );
    }

    #[test]
    fn conflict_do_update_on_auto_pk_reports_the_updated_id() {
        let model = with_pk(
            empty_model_with("t", &[("id", FieldType::I64), ("a", FieldType::String)]),
            "id",
            true,
        );
        let mut sql = String::new();
        MySql
            .write_conflict_clause(
                &mut sql,
                model,
                &[],
                &ConflictClause::DoUpdate {
                    target: vec![],
                    update_columns: vec!["a"],
                },
            )
            .unwrap();
        assert_eq!(
            sql,
            " ON DUPLICATE KEY UPDATE `a` = IF(LAST_INSERT_ID(`id`), VALUES(`a`), VALUES(`a`))"
        );
    }

    /// #2200: assigning the auto-increment PK fails with 1869 in a batch.
    #[test]
    fn conflict_do_nothing_on_auto_pk_assigns_another_column() {
        let model = with_pk(
            empty_model_with("t", &[("id", FieldType::I64), ("code", FieldType::String)]),
            "id",
            true,
        );
        let mut sql = String::new();
        MySql
            .write_conflict_clause(&mut sql, model, &[], &ConflictClause::DoNothing)
            .unwrap();
        assert_eq!(
            sql,
            " ON DUPLICATE KEY UPDATE `code` = IF(LAST_INSERT_ID(18446744073709551615), `code`, `code`)"
        );
    }

    #[test]
    fn conflict_do_update_with_empty_target_translates() {
        let model = empty_model_with("t", &[("a", FieldType::String), ("b", FieldType::String)]);
        let mut sql = String::new();
        MySql
            .write_conflict_clause(
                &mut sql,
                model,
                &[],
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
        // No other unique key can clash, so the target needs no clause.
        let model = empty_model_with("t", &[("id", FieldType::I64), ("a", FieldType::String)]);
        let mut sql = String::new();
        MySql
            .write_conflict_clause(
                &mut sql,
                model,
                &[],
                &ConflictClause::DoUpdate {
                    target: vec!["id"],
                    update_columns: vec!["a"],
                },
            )
            .expect("no other unique key, so the target is accepted");
        assert_eq!(sql, " ON DUPLICATE KEY UPDATE `a` = VALUES(`a`)");
    }

    /// A bound `Auto` PK can clash like any key.
    #[test]
    fn conflict_do_update_refuses_a_bound_auto_pk() {
        use crate::core::{InsertQuery, SqlValue};
        let base = with_pk(
            empty_model_with(
                "acct",
                &[("id", FieldType::I64), ("email", FieldType::String)],
            ),
            "id",
            true,
        );
        let fields: Vec<crate::core::FieldSchema> = base
            .fields
            .iter()
            .map(|f| crate::core::FieldSchema {
                unique: f.column == "email",
                ..*f
            })
            .collect();
        let model: &'static crate::core::ModelSchema =
            Box::leak(Box::new(crate::core::ModelSchema {
                fields: Box::leak(fields.into_boxed_slice()),
                ..*base
            }));
        let upsert = |columns: Vec<&'static str>, values| {
            InsertQuery::new(model, columns, values).on_conflict(ConflictClause::DoUpdate {
                target: vec!["email"],
                update_columns: vec!["email"],
            })
        };
        let email = || SqlValue::String("a@x".into());
        MySql
            .compile_insert(&upsert(vec!["email"], vec![email()]))
            .expect("an omitted Auto PK is generated, so it never clashes");
        let bound = MySql.compile_insert(&upsert(
            vec!["id", "email"],
            vec![SqlValue::I64(5), email()],
        ));
        assert!(
            matches!(bound, Err(SqlError::ConflictNotSupportedInDialect { .. })),
            "{bound:?}"
        );
    }

    /// A bound uuid PK clashes unless the writer marks it generated.
    #[test]
    fn conflict_do_update_trusts_only_a_generated_uuid_pk() {
        use crate::core::{InsertQuery, SqlValue};
        let base = with_pk(
            empty_model_with(
                "item",
                &[("id", FieldType::Uuid), ("name", FieldType::String)],
            ),
            "id",
            true,
        );
        let fields: Vec<crate::core::FieldSchema> = base
            .fields
            .iter()
            .map(|f| crate::core::FieldSchema {
                unique: f.column == "name",
                ..*f
            })
            .collect();
        let model: &'static crate::core::ModelSchema =
            Box::leak(Box::new(crate::core::ModelSchema {
                fields: Box::leak(fields.into_boxed_slice()),
                ..*base
            }));
        let q = InsertQuery::new(
            model,
            vec!["id", "name"],
            vec![
                SqlValue::Uuid(uuid::Uuid::nil()),
                SqlValue::String("a".into()),
            ],
        )
        .on_conflict(ConflictClause::DoUpdate {
            target: vec!["name"],
            update_columns: vec!["name"],
        });
        let set = MySql.compile_insert(&q);
        assert!(
            matches!(set, Err(SqlError::ConflictNotSupportedInDialect { .. })),
            "{set:?}"
        );
        MySql
            .compile_insert(&q.generated(vec!["id"]))
            .expect("a generated uuid PK never clashes");
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
            select_related: vec![],
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
            select_related: vec![],
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
            select_related: vec![],
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
            select_related: vec![],
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
            select_related: vec![],
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
            select_related: vec![],
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
            select_related: vec![],
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt
            .sql
            .contains("JSON_CONTAINS_PATH(`meta`, 'one', CONCAT('$.', JSON_QUOTE(?)))"));
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
            select_related: vec![],
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt
            .sql
            .contains("JSON_CONTAINS_PATH(`meta`, 'one', CONCAT('$.', JSON_QUOTE(?)), CONCAT('$.', JSON_QUOTE(?)))"));
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
            select_related: vec![],
        };
        let stmt = MySql.compile_select(&q).unwrap();
        assert!(stmt
            .sql
            .contains("JSON_CONTAINS_PATH(`meta`, 'all', CONCAT('$.', JSON_QUOTE(?)), CONCAT('$.', JSON_QUOTE(?)))"));
    }

    #[test]
    fn bulk_update_translates_to_inner_join_values_row() {
        use crate::core::{BulkUpdateQuery, SqlValue};
        let model = empty_model_with(
            "users",
            &[("id", FieldType::I64), ("name", FieldType::String)],
        );
        // Mark the id field as the PK so primary_key() resolves.
        let pk_model = with_pk(model, "id", false);
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
        assert!(stmt.sql.starts_with("UPDATE `users` INNER JOIN (/*!80000 VALUES "));
        assert!(stmt.sql.contains("ROW(?, ?), ROW(?, ?)"));
        assert!(stmt
            .sql
            .contains("/*M! SELECT ? AS `id`, ? AS `name` UNION ALL SELECT ?, ?) AS __data */"));
        assert!(stmt.sql.contains(") AS __data(`id`, `name`)"));
        assert!(stmt.sql.contains("ON `users`.`id` = __data.`id`"));
        assert!(stmt.sql.contains("SET `users`.`name` = __data.`name`"));
        assert_eq!(stmt.params.len(), 4);
    }

    /// Copy `model` with `pk_col` marked as the primary key, `auto`
    /// making it auto-increment.
    fn with_pk(
        model: &'static crate::core::ModelSchema,
        pk_col: &'static str,
        auto: bool,
    ) -> &'static crate::core::ModelSchema {
        let new_fields: Vec<crate::core::FieldSchema> = model
            .fields
            .iter()
            .map(|f| {
                let mut f = *f;
                if f.column == pk_col {
                    f.primary_key = true;
                    f.auto = auto;
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
            select_related: vec![],
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
            generated: vec![],
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
                auto_now: false,
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
