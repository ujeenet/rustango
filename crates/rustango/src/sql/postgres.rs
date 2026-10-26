//! Postgres dialect: double-quoted identifiers, `$1`-style placeholders,
//! `BIGSERIAL` / `SERIAL` for `Auto<T>` PKs, native `BOOLEAN`,
//! `pg_advisory_lock` for migration coordination, full `RETURNING` /
//! `ON CONFLICT` / `ILIKE` / `IS DISTINCT FROM` / JSONB operator support.
//!
//! The IR-to-SQL helpers live in [`super::writers`]. This module is
//! the thin per-dialect shell: identity primitives, the
//! Postgres-specific NULL-cast table, the conflict-clause spelling,
//! and `compile_*` methods that hand off to the writers.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, ConflictClause, CountQuery, DeleteQuery,
    FieldType, InsertQuery, ModelSchema, Op, SelectQuery, UpdateQuery,
};

use super::server::{AdminRight, Collation, DatabaseName, Login, LoginPassword, ServerAccount};
use super::writers::{
    write_aggregate, write_bulk_insert, write_bulk_update_pg, write_count, write_delete,
    write_insert, write_select, write_update, Sql,
};
use super::{CompiledStatement, Dialect, SqlError};

/// A string literal whose meaning does not depend on
/// `standard_conforming_strings`: `E'…'` once a `\` is present.
fn escape_string_literal(text: &str) -> String {
    let quoted = text.replace('\'', "''");
    if text.contains('\\') {
        format!("E'{}'", quoted.replace('\\', "\\\\"))
    } else {
        format!("'{quoted}'")
    }
}

/// The Postgres dialect.
///
/// Stateless; construct with `Postgres` and call [`Dialect::compile_select`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Postgres;

/// `'static` reference to the singleton [`Postgres`] dialect, useful
/// where callers want a `&'static dyn Dialect` (e.g. [`crate::sql::Pool::dialect`]).
#[cfg(feature = "postgres")]
pub static DIALECT: &Postgres = &Postgres;

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    // Postgres uses ANSI-style double-quoted identifiers — same as the
    // trait default, no override needed for `quote_ident`.

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    // PostgreSQL drops any constraint — CHECK, FOREIGN KEY, UNIQUE —
    // through the one ANSI spelling, and supports `IF EXISTS` on it, so
    // both of these are idempotent and identical. MySQL needs
    // `DROP CHECK` / `DROP FOREIGN KEY` and rejects `IF EXISTS` there;
    // SQLite has no `ALTER TABLE … DROP CONSTRAINT` at all and returns
    // `None`. These were the trait's default body until it became clear
    // that defaulting to the PG form is what shipped #559 to MySQL.
    fn drop_check_constraint_sql(&self, table: &str, name: &str) -> Option<String> {
        Some(format!(
            "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}",
            self.quote_ident(table),
            self.quote_ident(name)
        ))
    }

    fn reset_sequence_sql(&self, table: &str, column: &str) -> Option<String> {
        let (t, c) = (self.quote_ident(table), self.quote_ident(column));
        Some(format!(
            "SELECT setval(pg_get_serial_sequence({}, {}), COALESCE(MAX({c}), 1), \
             MAX({c}) IS NOT NULL) FROM {t}",
            self.placeholder(1),
            self.placeholder(2),
        ))
    }

    fn defer_foreign_keys_sql(&self) -> Option<&'static str> {
        None
    }

    /// A Postgres database inherits its collation from the template.
    fn create_database_sql(&self, db: &DatabaseName, _: Option<&Collation>) -> Option<String> {
        Some(format!("CREATE DATABASE {}", self.quote_ident(db.as_str())))
    }

    fn drop_database_sql(&self, db: &DatabaseName) -> Option<String> {
        Some(format!(
            "DROP DATABASE IF EXISTS {}",
            self.quote_ident(db.as_str())
        ))
    }

    /// Postgres 13+.
    fn force_drop_database_sql(&self, db: &DatabaseName) -> Option<String> {
        Some(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.quote_ident(db.as_str())
        ))
    }

    fn database_exists_sql(&self, db: &DatabaseName) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: format!(
                "SELECT count(*) FROM pg_database WHERE datname = {}",
                self.placeholder(1)
            ),
            params: vec![crate::core::SqlValue::String(db.as_str().to_owned())],
        })
    }

    fn login_exists_sql(&self, login: &Login) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: format!(
                "SELECT count(*) FROM pg_roles WHERE rolname = {}",
                self.placeholder(1)
            ),
            params: vec![crate::core::SqlValue::String(
                login.name().as_str().to_owned(),
            )],
        })
    }

    /// A SCRAM verifier where it can be built, so the server log never
    /// holds the password; else the text in a mode-proof literal.
    fn create_login_sql(&self, login: &Login, password: &LoginPassword) -> Option<String> {
        Some(format!(
            "CREATE ROLE {} LOGIN PASSWORD {}",
            self.quote_ident(login.name().as_str()),
            escape_string_literal(&password.pg_secret())
        ))
    }

    fn drop_login_sql(&self, login: &Login) -> Option<String> {
        Some(format!(
            "DROP ROLE IF EXISTS {}",
            self.quote_ident(login.name().as_str())
        ))
    }

    /// No dual passwords: the old one stops working at once.
    fn alter_login_password_sql(
        &self,
        login: &Login,
        password: &LoginPassword,
        _retain_current: bool,
    ) -> Option<String> {
        Some(format!(
            "ALTER ROLE {} PASSWORD {}",
            self.quote_ident(login.name().as_str()),
            escape_string_literal(&password.pg_secret())
        ))
    }

    /// PG 16+ lets a CREATEROLE login alter or drop only roles it holds
    /// ADMIN OPTION on, so a replaced admin may not touch older logins.
    /// Ending sessions needs membership or `pg_signal_backend`.
    fn tenant_rights_sql(&self, db: &DatabaseName, login: &Login) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: "SELECT \
                  (SELECT me.rolsuper OR pg_has_role(current_user, d.datdba, 'USAGE') \
                     FROM pg_database d WHERE d.datname = $1), \
                  (SELECT CASE WHEN current_setting('server_version_num')::int >= 160000 \
                     THEN pg_has_role(current_user, t.oid, 'MEMBER WITH ADMIN OPTION') \
                     ELSE me.rolsuper OR me.rolcreaterole END \
                     FROM pg_roles t WHERE t.rolname = $2), \
                  me.rolsuper OR pg_has_role(current_user, 'pg_signal_backend', 'USAGE') \
                     OR COALESCE((SELECT pg_has_role(current_user, t.oid, 'USAGE') \
                     FROM pg_roles t WHERE t.rolname = $2), false) \
                  FROM pg_roles me WHERE me.rolname = current_user"
                .to_owned(),
            params: vec![
                crate::core::SqlValue::String(db.as_str().to_owned()),
                crate::core::SqlValue::String(login.name().as_str().to_owned()),
            ],
        })
    }

    fn database_sessions_sql(&self, db: &DatabaseName, tag: &str) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: "SELECT count(*) FILTER (WHERE application_name = $2), \
                  count(*) FILTER (WHERE application_name IS DISTINCT FROM $2) \
                  FROM pg_stat_activity WHERE datname = $1"
                .to_owned(),
            params: vec![
                crate::core::SqlValue::String(db.as_str().to_owned()),
                crate::core::SqlValue::String(tag.to_owned()),
            ],
        })
    }

    fn end_sessions_sql(&self, db: &DatabaseName, tag: &str) -> Option<CompiledStatement> {
        Some(CompiledStatement {
            sql: "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
                  WHERE datname = $1 AND application_name = $2"
                .to_owned(),
            params: vec![
                crate::core::SqlValue::String(db.as_str().to_owned()),
                crate::core::SqlValue::String(tag.to_owned()),
            ],
        })
    }

    /// PUBLIC has CONNECT on every new database; revoking it keeps the
    /// other tenants' logins out.
    fn grant_database_sql(&self, db: &DatabaseName, login: &Login) -> Vec<String> {
        let (d, u) = (
            self.quote_ident(db.as_str()),
            self.quote_ident(login.name().as_str()),
        );
        vec![
            format!("REVOKE ALL ON DATABASE {d} FROM PUBLIC"),
            format!("GRANT ALL PRIVILEGES ON DATABASE {d} TO {u}"),
        ]
    }

    /// PG 15+ gives `public` CREATE to the database owner only, which is
    /// the admin, not the tenant login.
    fn grant_in_database_sql(&self, login: &Login) -> Vec<String> {
        vec![format!(
            "GRANT ALL ON SCHEMA public TO {}",
            self.quote_ident(login.name().as_str())
        )]
    }

    fn server_version_sql(&self) -> Option<&'static str> {
        Some("SELECT current_setting('server_version')")
    }

    /// The cluster's `system_identifier`; any role may read it.
    fn database_identity_sql(&self, _version: &str, db: Option<&str>) -> Option<CompiledStatement> {
        let (database, params) = match db {
            Some(db) => (
                format!("{}::text", self.placeholder(1)),
                vec![crate::core::SqlValue::String(db.to_owned())],
            ),
            None => ("current_database()::text".to_owned(), Vec::new()),
        };
        Some(CompiledStatement {
            sql: format!("SELECT system_identifier::text, {database} FROM pg_control_system()"),
            params,
        })
    }

    /// One row: `rolsuper`, `rolcreatedb`, `rolcreaterole`.
    fn login_privileges_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT rolsuper, rolcreatedb, rolcreaterole FROM pg_roles \
             WHERE rolname = current_user",
        )
    }

    fn current_account_sql(&self) -> Option<&'static str> {
        Some("SELECT current_user::text")
    }

    fn schema_privileges_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT current_user::text, current_database()::text, \
             has_database_privilege(current_user, current_database(), 'CREATE')",
        )
    }

    /// The creator owns the new database, so `CREATEDB` covers the grant.
    fn grant_admin_rights_sql(
        &self,
        account: &ServerAccount,
        database: Option<&str>,
        rights: &[AdminRight],
    ) -> Vec<String> {
        let role = self.quote_ident(&account.user);
        let mut attrs = Vec::new();
        if rights.contains(&AdminRight::CreateDatabase) || rights.contains(&AdminRight::Grant) {
            attrs.push("CREATEDB");
        }
        if rights.contains(&AdminRight::CreateLogin) {
            attrs.push("CREATEROLE");
        }
        let mut out = Vec::new();
        if !attrs.is_empty() {
            out.push(format!("ALTER ROLE {role} {}", attrs.join(" ")));
        }
        if let (true, Some(db)) = (rights.contains(&AdminRight::CreateSchema), database) {
            out.push(format!(
                "GRANT CREATE ON DATABASE {} TO {role}",
                self.quote_ident(db)
            ));
        }
        out
    }

    fn clear_tables_sql(&self, tables: &[&str]) -> Vec<String> {
        let quoted: Vec<String> = tables.iter().map(|t| self.quote_ident(t)).collect();
        vec![format!(
            "TRUNCATE TABLE {} RESTART IDENTITY CASCADE",
            quoted.join(", ")
        )]
    }

    fn drop_foreign_key_sql(&self, table: &str, name: &str) -> Option<String> {
        Some(format!(
            "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}",
            self.quote_ident(table),
            self.quote_ident(name)
        ))
    }

    /// `to_regclass` resolves the table through `search_path`, as the DDL does.
    fn foreign_key_names_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT c.conname::text FROM pg_constraint c \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey) \
             WHERE c.contype = 'f' AND c.conrelid = to_regclass(quote_ident($1)) \
             AND a.attname = $2 AND cardinality(c.conkey) = 1",
        )
    }

    /// A renamed column keeps its UNIQUE constraint's old name (#2133).
    fn unique_index_names_sql(&self) -> Option<&'static str> {
        Some(
            "SELECT c.conname::text FROM pg_constraint c \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey) \
             WHERE c.contype = 'u' AND c.conrelid = to_regclass(quote_ident($1)) \
             AND a.attname = $2 AND cardinality(c.conkey) = 1",
        )
    }

    fn drop_unique_index_sql(&self, table: &str, name: &str) -> Option<String> {
        Some(format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            self.quote_ident(table),
            self.quote_ident(name)
        ))
    }

    fn column_comment_statement(&self, table: &str, column: &str, comment: &str) -> Option<String> {
        Some(format!(
            "COMMENT ON COLUMN {}.{} IS {}",
            self.quote_ident(table),
            self.quote_ident(column),
            self.quote_literal(comment),
        ))
    }

    fn table_comment_statement(&self, table: &str, comment: &str) -> Option<String> {
        Some(format!(
            "COMMENT ON TABLE {} IS {}",
            self.quote_ident(table),
            self.quote_literal(comment),
        ))
    }

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "SERIAL",
            _ => "BIGSERIAL",
        }
    }

    // #344 — CITextField. Postgres ships the `citext` extension that
    // provides a case-insensitive text type; once the extension is
    // installed, a `CITEXT` column compares case-insensitively. Migrations
    // run `ci_text_extension_sql` before the first CITEXT column (#2240).
    // In `public`, so every tenant's `search_path` finds it (#2269).
    fn ci_text_type(&self, _max_length: Option<u32>) -> String {
        // CITEXT has no length parameter; `max_length` is advisory.
        "CITEXT".to_owned()
    }

    fn ci_text_extension_sql(&self) -> Option<&'static str> {
        Some("CREATE EXTENSION IF NOT EXISTS citext SCHEMA public;")
    }

    fn ci_text_bind_cast(&self) -> Option<&'static str> {
        Some("citext")
    }

    // Postgres has a native `BOOLEAN` type with `TRUE` / `FALSE`
    // literals — same as the trait default, no override.

    fn supports_concurrent_index(&self) -> bool {
        true
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn supports_dml_in_cte(&self) -> bool {
        true
    }

    fn supports(&self, token: &str) -> bool {
        // Postgres-specific capability tokens that aren't reachable via
        // the default impl's whitelist. Extends the default; the
        // generic `window_functions` / `cte` / etc. fall through to
        // `super::Dialect::supports` via the `||`.
        matches!(
            token,
            "array_type"
                | "range_type"
                | "hstore"
                | "citext"
                | "listen_notify"
                | "notify"
                | "row_security"
                | "gin_index"
                | "gist_index"
                | "spgist_index"
                | "brin_index"
                | "unique_constraint_deferred"
                | "exclusion_constraint"
                | "tablespaces"
                | "json_path"
                | "json_query"
                // pgvector similarity search (#824). Requires the
                // `vector` extension to be installed/enabled.
                | "pgvector"
                | "vector"
                // PostGIS geometry columns (#443). Requires the
                // `postgis` extension to be installed/enabled.
                | "postgis"
                | "geometry"
        ) || self.default_supports(token)
    }

    fn cast_aggregate_to_int(&self, expr: &str) -> String {
        // PostgreSQL accepts the shorter `<expr>::bigint` form.
        format!("{expr}::bigint")
    }

    fn cast_aggregate_to_float(&self, expr: &str) -> String {
        format!("{expr}::double precision")
    }

    fn null_cast(&self, ty: FieldType) -> Option<&'static str> {
        // #562 — the PG `null_cast` table was character-identical to
        // the trait-default `cast_type` table 14 variants long.
        // Delegate so the canonical token table lives in one place
        // (`Dialect::cast_type` default impl); divergence between
        // the two would silently change which `NULL::T` token PG
        // sees.
        self.cast_type(ty)
    }

    /// Postgres supports every `Op` the IR carries — `ILIKE`,
    /// `IS DISTINCT FROM`, JSONB operators, etc. (Trait default
    /// `true` is correct; the explicit override documents intent.)
    fn supports_op(&self, _op: Op) -> bool {
        true
    }

    /// `bigint LIKE text` has no operator, so a non-text column is
    /// cast first (#2229, #2263).
    fn like_operand(&self, qualified_col: &str, ty: Option<FieldType>) -> String {
        match ty {
            Some(ty) if ty != FieldType::String => format!("CAST({qualified_col} AS TEXT)"),
            _ => qualified_col.to_owned(),
        }
    }

    fn write_conflict_clause(
        &self,
        sql: &mut String,
        _model: &ModelSchema,
        _columns: &[&str],
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
                    write_pg_ident(sql, col);
                }
                sql.push_str(") DO UPDATE SET ");
                let mut first = true;
                for col in update_columns {
                    if !first {
                        sql.push_str(", ");
                    }
                    first = false;
                    write_pg_ident(sql, col);
                    sql.push_str(" = EXCLUDED.");
                    write_pg_ident(sql, col);
                }
            }
        }
        Ok(())
    }

    fn acquire_session_lock_sql(&self) -> Option<String> {
        Some(format!(
            "SELECT pg_try_advisory_lock({})",
            self.placeholder(1)
        ))
    }

    fn release_session_lock_sql(&self) -> Option<String> {
        Some(format!(
            "SELECT pg_advisory_unlock({})",
            self.placeholder(1)
        ))
    }

    fn acquire_xact_lock_sql(&self) -> Option<String> {
        Some(format!(
            "SELECT pg_advisory_xact_lock({})",
            self.placeholder(1)
        ))
    }

    // ---- compilation: thin shells over `writers::*` ----

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
        let mut b = Sql::new(self);
        write_bulk_update_pg(&mut b, query)?;
        b.finish()
    }
}

/// Direct Postgres-quoted identifier writer used by
/// [`Postgres::write_conflict_clause`]. The conflict clause writes
/// directly into a `&mut String` (it's part of the `Dialect` trait
/// surface, not a `Sql<'_>` builder), so we need a small helper that
/// doesn't go through the builder.
fn write_pg_ident(sql: &mut String, name: &str) {
    sql.push('"');
    for ch in name.chars() {
        if ch == '"' {
            sql.push_str("\"\"");
        } else {
            sql.push(ch);
        }
    }
    sql.push('"');
}
