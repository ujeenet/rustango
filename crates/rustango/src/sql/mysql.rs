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
    FieldType, InsertQuery, Op, SelectQuery, UpdateQuery,
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

    /// `MySQL` quotes identifiers with backticks, not double quotes.
    /// Embedded backticks are doubled (the `MySQL` parser's escape rule)
    /// so the output is always a valid quoted identifier even for
    /// pathological column names.
    fn quote_ident(&self, name: &str) -> String {
        let escaped = name.replace('`', "``");
        format!("`{escaped}`")
    }

    // `?`-style placeholders are the trait default — no override needed.

    fn serial_type(&self, field_type: FieldType) -> &'static str {
        match field_type {
            FieldType::I32 => "INT AUTO_INCREMENT",
            _ => "BIGINT AUTO_INCREMENT",
        }
    }

    /// `MySQL` has no native `BOOLEAN` (the `BOOL` keyword is just an
    /// alias for `TINYINT(1)`). Emit `1`/`0` so `DEFAULT` clauses and
    /// inline comparisons match the storage shape.
    fn bool_literal(&self, b: bool) -> &'static str {
        if b { "1" } else { "0" }
    }

    /// `MySQL` rejects `ILIKE`, `IS DISTINCT FROM`, and the JSONB
    /// operators that have no native equivalent. Returning `false`
    /// here makes the writer fast-fail with a clear
    /// [`SqlError::OperatorNotSupportedInDialect`] instead of producing
    /// SQL the parser would reject. Translation lands in batch4.
    fn supports_op(&self, op: Op) -> bool {
        !matches!(
            op,
            Op::ILike
                | Op::NotILike
                | Op::IsDistinctFrom
                | Op::IsNotDistinctFrom
                | Op::JsonContains
                | Op::JsonContainedBy
                | Op::JsonHasKey
                | Op::JsonHasAnyKey
                | Op::JsonHasAllKeys
        )
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
                if !target.is_empty() {
                    return Err(SqlError::ConflictNotSupportedInDialect {
                        shape: "DO UPDATE with target columns",
                        dialect: self.name(),
                    });
                }
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

    fn compile_bulk_insert(
        &self,
        query: &BulkInsertQuery,
    ) -> Result<CompiledStatement, SqlError> {
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

    fn compile_bulk_update(
        &self,
        _query: &BulkUpdateQuery,
    ) -> Result<CompiledStatement, SqlError> {
        // Postgres uses `UPDATE … FROM (VALUES …)`; MySQL has no such
        // shape pre-8.0.19. Batch4 will translate to either a
        // `JOIN (VALUES …)` or a `CASE WHEN` cascade. Until then,
        // fail clearly so callers know to fall back to per-row
        // `compile_update`.
        Err(SqlError::DialectQueryCompilationNotImplemented { dialect: "mysql" })
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
    fn supports_op_rejects_pg_only_operators() {
        assert!(!MySql.supports_op(Op::ILike));
        assert!(!MySql.supports_op(Op::NotILike));
        assert!(!MySql.supports_op(Op::IsDistinctFrom));
        assert!(!MySql.supports_op(Op::IsNotDistinctFrom));
        assert!(!MySql.supports_op(Op::JsonContains));
        assert!(!MySql.supports_op(Op::JsonContainedBy));
        assert!(!MySql.supports_op(Op::JsonHasKey));
        assert!(!MySql.supports_op(Op::JsonHasAnyKey));
        assert!(!MySql.supports_op(Op::JsonHasAllKeys));
    }

    #[test]
    fn supports_op_accepts_portable_operators() {
        assert!(MySql.supports_op(Op::Eq));
        assert!(MySql.supports_op(Op::Ne));
        assert!(MySql.supports_op(Op::Like));
        assert!(MySql.supports_op(Op::In));
        assert!(MySql.supports_op(Op::Between));
        assert!(MySql.supports_op(Op::IsNull));
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
    fn conflict_do_update_with_target_errors() {
        let mut sql = String::new();
        let err = MySql
            .write_conflict_clause(
                &mut sql,
                &ConflictClause::DoUpdate {
                    target: vec!["id"],
                    update_columns: vec!["a"],
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            SqlError::ConflictNotSupportedInDialect { dialect: "mysql", .. }
        ));
    }

    #[test]
    fn bulk_update_errors_until_batch4() {
        use crate::core::{BulkUpdateQuery, ModelSchema};
        // We don't need to construct a real BulkUpdateQuery — the
        // dispatch fast-fails before touching the IR. Use a stub.
        let stub = BulkUpdateQuery {
            model: empty_model(),
            update_columns: vec!["x"],
            rows: vec![],
        };
        let err = MySql.compile_bulk_update(&stub).unwrap_err();
        assert!(matches!(
            err,
            SqlError::DialectQueryCompilationNotImplemented { dialect: "mysql" }
        ));
        let _ = std::any::type_name::<ModelSchema>();
    }

    fn empty_model() -> &'static crate::core::ModelSchema {
        empty_model_with("stub", &[])
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

    #[test]
    fn ilike_filter_errors_with_clear_message() {
        use crate::core::{Filter, Op, SelectQuery, SqlValue, WhereExpr};
        let model = empty_model_with("users", &[("name", FieldType::String)]);
        let q = SelectQuery {
            model,
            joins: vec![],
            where_clause: WhereExpr::Predicate(Filter {
                column: "name",
                op: Op::ILike,
                value: SqlValue::String("%a%".into()),
            }),
            search: None,
            order_by: vec![],
            limit: None,
            offset: None,
        };
        let err = MySql.compile_select(&q).unwrap_err();
        assert!(matches!(
            err,
            SqlError::OperatorNotSupportedInDialect {
                op: "ILIKE",
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
        }))
    }
}
