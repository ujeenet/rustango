#![cfg(all(feature = "mysql", feature = "postgres"))]
//! Compile-time check that `#[derive(Model)]` emits an
//! `impl FromRow<MySqlRow>` when rustango is built with the `mysql`
//! feature. The check is the type assertion alone — if the proc-macro
//! → `__impl_my_from_row!` → `impl<'r> FromRow<'r, MySqlRow>` chain
//! breaks, this test fails to compile.
//!
//! Skipped under PG-only builds via `#![cfg(feature = "mysql")]`. The
//! existing `tests/derive_model.rs` covers the `FromRow<PgRow>` side
//! end-to-end.

#![cfg(feature = "mysql")]

use rustango::Model;

#[derive(Model)]
#[rustango(table = "mysql_from_row_users")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    name: String,
    email: Option<String>,
    is_active: bool,
}

#[derive(Model)]
#[rustango(table = "mysql_from_row_posts")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    title: String,
    body: String,
}

fn assert_my_from_row<T>()
where
    T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
{
}

#[test]
fn user_model_implements_my_from_row() {
    assert_my_from_row::<User>();
    assert_my_from_row::<Post>();
}

#[test]
fn user_model_also_implements_pg_from_row() {
    // Regression guard: the macro must still emit the PG impl
    // alongside the MySQL one — the `__impl_my_from_row!` call sits
    // *after* the existing `impl FromRow<PgRow>`, but a refactor
    // that accidentally replaces (vs. adds) would silently break PG.
    fn assert_pg_from_row<T>()
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
    {
    }
    assert_pg_from_row::<User>();
    assert_pg_from_row::<Post>();
}

#[test]
fn maybe_my_from_row_resolves_for_derived_model() {
    // The MaybeMyFromRow bound is what `select_rows_pool` and
    // `FetcherPool::fetch_pool` use. Confirm derived models satisfy
    // it under the mysql feature config.
    fn check<T: rustango::sql::MaybeMyFromRow>() {}
    check::<User>();
    check::<Post>();
}

#[test]
fn delete_pool_method_emitted_for_non_audited_model() {
    // Compile-time check: the macro emits `delete_pool(&self, &Pool) -> impl Future`
    // for every non-audited model with a primary key. We don't await
    // (would need a live DB); just calling the method confirms it
    // resolves with the right signature.
    fn _probe(u: &User, p: &Post, pool: &rustango::sql::Pool) {
        let _fut = u.delete_pool(pool);
        let _fut = p.delete_pool(pool);
    }
}

#[test]
fn aliased_my_row_decoder_emitted_for_derived_model() {
    // batch 8 — `__rustango_from_aliased_my_row(row, prefix)` is the
    // MySQL counterpart of `__rustango_from_aliased_row`. The proc
    // macro emits both via the cfg-gated macro_rules. Resolve the
    // function pointer to confirm presence + signature.
    fn _probe(row: &sqlx::mysql::MySqlRow, prefix: &str) {
        let _r: Result<User, _> = User::__rustango_from_aliased_my_row(row, prefix);
        let _r: Result<Post, _> = Post::__rustango_from_aliased_my_row(row, prefix);
    }
}

#[test]
fn maybe_my_load_related_resolves_for_derived_model() {
    // The MaybeMyLoadRelated marker is what future _pool join-decoding
    // executor functions will bound on. Even FK-less models satisfy it
    // because the proc macro emits an empty-arms LoadRelatedMy impl
    // (mirroring how PG's LoadRelated is universally implemented).
    fn check<T: rustango::sql::MaybeMyLoadRelated>() {}
    check::<User>();
    check::<Post>();
}

#[test]
fn insert_pool_and_save_pool_methods_emitted() {
    // batch 9 — the macro emits `insert_pool(&Pool)` and
    // `save_pool(&mut self, &Pool)` for non-audited models with PKs.
    // The Auto-bearing branch returns the future borrowing &mut self;
    // the non-Auto branch borrows &self. Compile-time probe only —
    // no live DB. Each future is dropped before the next call so the
    // borrows don't overlap.
    fn _probe(u: &mut User, p: &mut Post, pool: &rustango::sql::Pool) {
        drop(u.insert_pool(pool));
        drop(p.insert_pool(pool));
        drop(u.save_pool(pool));
        drop(p.save_pool(pool));
    }
}

#[test]
fn ddl_create_table_emits_mysql_shape() {
    // batch 10 — DDL writer dispatches through Dialect.
    // Verify MySQL emits backticks, TINYINT(1) for bool, TEXT for
    // unbounded string, BIGINT for i64.
    use rustango::core::Model;
    use rustango::migrate::ddl::create_table_sql_with_dialect;
    use rustango::sql::MySql;

    let sql = create_table_sql_with_dialect(&MySql, <User as Model>::SCHEMA);
    // MySQL identifier quoting
    assert!(sql.starts_with("CREATE TABLE `mysql_from_row_users` ("));
    // BIGINT for i64 PK (no Auto<T> on this test model — plain BIGINT)
    assert!(sql.contains("`id` BIGINT"));
    // TEXT for unbounded String
    assert!(sql.contains("`name` TEXT"));
    // No PG-isms
    assert!(!sql.contains("\""));
    assert!(!sql.contains("BIGSERIAL"));
}

#[test]
fn ddl_create_table_pg_unchanged() {
    // Regression guard: PG-typed shim still emits identical bytes
    // to the pre-batch10 surface.
    use rustango::core::Model;
    use rustango::migrate::ddl::create_table_sql;

    let sql = create_table_sql(<User as Model>::SCHEMA);
    assert!(sql.starts_with("CREATE TABLE \"mysql_from_row_users\" ("));
    assert!(sql.contains("\"id\" BIGINT"));
    assert!(sql.contains("\"name\" TEXT"));
    assert!(!sql.contains("`"));
}

#[test]
fn apply_all_pool_and_drop_all_pool_are_callable() {
    // batch 11 — apply_all_pool / drop_all_pool take &Pool and
    // dispatch through Dialect for both DDL emission and execution.
    // Compile-time probe — the functions are async and would dial
    // the connect_lazy URL only on first execute, so we don't
    // actually run them.
    fn _probe(pool: &rustango::sql::Pool) {
        let _fut = rustango::migrate::apply_all_pool(pool);
        let _fut = rustango::migrate::drop_all_pool(pool);
    }
}

#[test]
fn ledger_pool_runner_surface_is_callable() {
    // batch 12 — ensure_ledger_pool / applied_set_pool / migrate_pool
    // make the file-based ledger runner work against either backend.
    // Compile-time probe; live exec needs a database.
    use std::path::Path;
    fn _probe(pool: &rustango::sql::Pool, dir: &Path) {
        let _fut = rustango::migrate::ensure_ledger_pool(pool);
        let _fut = rustango::migrate::applied_set_pool(pool);
        let _fut = rustango::migrate::migrate_pool(pool, dir);
    }
}

#[test]
fn direction_aware_pool_runners_are_callable() {
    // batch 14 — migrate_to / unapply / unapply_force / downgrade /
    // migrate_dry_run all have _pool variants now.
    use std::path::Path;
    fn _probe(pool: &rustango::sql::Pool, dir: &Path) {
        let _fut = rustango::migrate::migrate_to_pool(pool, dir, "0001_init");
        let _fut = rustango::migrate::unapply_pool(pool, dir, "0001_init");
        let _fut = rustango::migrate::unapply_force_pool(pool, dir, "0001_init");
        let _fut = rustango::migrate::downgrade_pool(pool, dir, 1);
        let _fut = rustango::migrate::migrate_dry_run_pool(pool, dir);
    }
}

#[test]
fn fetcher_pool_satisfies_join_bound() {
    // batch 15 — FetcherPool::fetch_pool now requires LoadRelated +
    // MaybeMyLoadRelated alongside FromRow + MaybeMyFromRow. Every
    // derived Model satisfies all four bounds (FK-less models get
    // empty-arm impls), so this resolves at compile time.
    use rustango::sql::FetcherPool;
    fn _probe(pool: &rustango::sql::Pool) {
        let _fut = User::objects().fetch_pool(pool);
        let _fut = Post::objects().fetch_pool(pool);
    }
}

#[test]
fn select_rows_pool_with_related_is_callable() {
    // batch 15 — direct executor entry for join-aware fetch.
    use rustango::core::Model;
    use rustango::core::SelectQuery;
    fn _probe(pool: &rustango::sql::Pool) {
        let q = SelectQuery {
            model: <User as Model>::SCHEMA,
            joins: vec![],
            subquery_joins: Vec::new(),
            where_clause: rustango::core::WhereExpr::And(vec![]),
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
        let _fut: _ = rustango::sql::select_rows_pool_with_related::<User>(pool, &q);
    }
}

#[test]
fn audit_ensure_table_pool_and_emit_one_pool_are_callable() {
    // batch 16 — bi-dialect audit primitives. Compile-time probe;
    // execution would dial the pool and write to rustango_audit_log.
    fn _probe(pool: &rustango::sql::Pool) {
        let entry = rustango::audit::PendingEntry {
            entity_table: "users",
            entity_pk: "1".into(),
            operation: rustango::audit::AuditOp::Update,
            source: rustango::audit::AuditSource::System,
            changes: serde_json::json!({}),
        };
        let _fut = rustango::audit::ensure_table_pool(pool);
        let _fut = rustango::audit::emit_one_pool(pool, &entry);
    }
}

#[derive(Model)]
#[rustango(table = "mysql_from_row_audited", audit(track = "name"))]
#[allow(dead_code)]
pub struct AuditedRecord {
    #[rustango(primary_key)]
    id: i64,
    name: String,
}

#[test]
fn audited_model_gets_delete_pool() {
    // batch 20 — audited models now get delete_pool too. The macro
    // routes through audit::delete_one_with_audit which opens
    // a per-backend tx wrapping DELETE + audit emit. Compile-time
    // probe; live exec needs a database.
    fn _probe(rec: &AuditedRecord, pool: &rustango::sql::Pool) {
        let _fut = rec.delete_pool(pool);
    }
}

#[test]
fn audited_model_with_plain_pk_gets_save_pool() {
    // batch 21 — audited non-Auto-PK models get save_pool routing
    // through audit::save_one_with_audit (per-backend tx wraps
    // UPDATE + audit emit atomically; snapshot-style audit).
    fn _probe(rec: &mut AuditedRecord, pool: &rustango::sql::Pool) {
        let _fut = rec.save_pool(pool);
    }
}

#[derive(Model)]
#[rustango(table = "mysql_from_row_audited_auto", audit(track = "name"))]
#[allow(dead_code)]
pub struct AuditedAutoRecord {
    #[rustango(primary_key)]
    id: rustango::sql::Auto<i64>,
    name: String,
}

#[test]
fn audited_auto_pk_model_gets_insert_pool() {
    // batch 22 — audited Auto-PK models get insert_pool routing
    // through audit::insert_one_with_audit (per-backend tx
    // wraps INSERT + auto-PK readback + audit emit).
    fn _probe(rec: &mut AuditedAutoRecord, pool: &rustango::sql::Pool) {
        drop(rec.insert_pool(pool));
        drop(rec.save_pool(pool));
    }
}

#[test]
fn counter_pool_count_pool_is_callable() {
    // batch 24 — QuerySet::count_pool fills the QuerySet counter gap.
    use rustango::sql::CounterPool;
    fn _probe(pool: &rustango::sql::Pool) {
        let _fut = User::objects().count_pool(pool);
    }
}

#[test]
fn fetch_aggregate_pool_is_callable() {
    // batch 24 — bi-dialect aggregate fetch via &Pool.
    use rustango::core::{AggregateQuery, Model, WhereExpr};
    fn _probe(pool: &rustango::sql::Pool) {
        let q = AggregateQuery {
            model: <User as Model>::SCHEMA,
            where_clause: WhereExpr::And(vec![]),
            group_by: vec![],
            aggregates: vec![],
            aliases: vec![],
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
        };
        let _fut: _ = rustango::sql::fetch_aggregate_pool::<(i64,)>(pool, &q);
    }
}

#[test]
fn raw_query_pool_is_callable() {
    // batch 24 — raw SQL escape hatch via &Pool. Caller picks $1 / ?
    // placeholder shape per backend.
    fn _probe(pool: &rustango::sql::Pool) {
        let binds: Vec<rustango::core::SqlValue> = vec![];
        let _fut: _ = rustango::sql::raw_query_pool::<User>(
            "SELECT id, name, email, is_active FROM mysql_from_row_users",
            binds,
            pool,
        );
    }
}

#[test]
fn transaction_pool_returns_pool_tx() {
    // batch 23 — transaction_pool opens a tx, returns a PoolTx the
    // caller commits or rolls back. Compile-time probe.
    fn _probe(pool: &rustango::sql::Pool) {
        let _fut = rustango::sql::transaction_pool(pool);
    }
}

#[test]
fn pool_tx_commit_and_rollback_signatures() {
    // batch 23 — PoolTx::commit / rollback consume the tx. Compile-time probe.
    async fn _probe(pool: &rustango::sql::Pool) -> Result<(), rustango::sql::ExecError> {
        let tx = rustango::sql::transaction_pool(pool).await?;
        tx.commit().await?;
        let tx2 = rustango::sql::transaction_pool(pool).await?;
        tx2.rollback().await?;
        Ok(())
    }
}

#[test]
fn audited_plain_pk_model_gets_insert_pool() {
    // batch 22 — audited non-Auto-PK models also get insert_pool now.
    fn _probe(rec: &mut AuditedRecord, pool: &rustango::sql::Pool) {
        let _fut = rec.insert_pool(pool);
    }
}

#[test]
fn audit_insert_one_with_audit_pool_is_callable() {
    use rustango::core::{InsertQuery, Model, SqlValue};
    fn _probe(pool: &rustango::sql::Pool) {
        let q = InsertQuery {
            model: <AuditedAutoRecord as Model>::SCHEMA,
            columns: vec!["name"],
            values: vec![SqlValue::String("seed".into())],
            returning: vec!["id"],
            on_conflict: None,
        };
        let entry = rustango::audit::PendingEntry {
            entity_table: "mysql_from_row_audited_auto",
            entity_pk: String::new(),
            operation: rustango::audit::AuditOp::Create,
            source: rustango::audit::AuditSource::System,
            changes: serde_json::json!({}),
        };
        let _fut = rustango::audit::insert_one_with_audit(pool, &q, &entry);
    }
}

#[test]
fn audit_save_one_with_audit_pool_is_callable() {
    use rustango::core::{Filter, Model, Op, SqlValue, UpdateQuery, WhereExpr};
    fn _probe(pool: &rustango::sql::Pool) {
        let q = UpdateQuery {
            model: <AuditedRecord as Model>::SCHEMA,
            set: vec![rustango::core::Assignment {
                column: "name",
                value: SqlValue::String("changed".into()).into(),
            }],
            where_clause: WhereExpr::Predicate(Filter {
                column: "id",
                op: Op::Eq,
                value: SqlValue::I64(1),
            }),
        };
        let entry = rustango::audit::PendingEntry {
            entity_table: "mysql_from_row_audited",
            entity_pk: "1".into(),
            operation: rustango::audit::AuditOp::Update,
            source: rustango::audit::AuditSource::System,
            changes: serde_json::json!({}),
        };
        let _fut = rustango::audit::save_one_with_audit(pool, &q, &entry);
    }
}

#[test]
fn audit_delete_one_with_audit_pool_is_callable() {
    use rustango::core::{DeleteQuery, Filter, Model, Op, SqlValue, WhereExpr};
    fn _probe(pool: &rustango::sql::Pool) {
        let q = DeleteQuery {
            model: <AuditedRecord as Model>::SCHEMA,
            where_clause: WhereExpr::Predicate(Filter {
                column: "id",
                op: Op::Eq,
                value: SqlValue::I64(1),
            }),
        };
        let entry = rustango::audit::PendingEntry {
            entity_table: "mysql_from_row_audited",
            entity_pk: "1".into(),
            operation: rustango::audit::AuditOp::Delete,
            source: rustango::audit::AuditSource::System,
            changes: serde_json::json!({}),
        };
        let _fut = rustango::audit::delete_one_with_audit(pool, &q, &entry);
    }
}

#[test]
fn fetch_paginated_pool_is_callable() {
    // batch 19 — single-round-trip page + total via COUNT(*) OVER ().
    fn _probe(pool: &rustango::sql::Pool) {
        let qs = User::objects();
        let _fut = rustango::sql::fetch_paginated_pool::<User>(qs, pool);
    }
}

#[test]
fn fetch_with_prefetch_pool_is_callable() {
    // batch 18 — prefetch_related (1:N hydration) on either backend.
    // Compile-time probe; live exec needs a database. Use User as
    // the parent + Post as the child stand-in (test types have no
    // FK between them; the call would return an error at runtime
    // because the synthetic child_fk_column doesn't exist on Post,
    // but the bound resolution + signature is what we're checking).
    fn _probe(pool: &rustango::sql::Pool) {
        let parent_qs = User::objects();
        let _fut: _ =
            rustango::sql::fetch_with_prefetch_pool::<User, Post>(parent_qs, "user_id", pool);
    }
}

#[test]
fn migrate_embedded_pool_is_callable() {
    // batch 17 — single-binary distribution path on either backend.
    fn _probe(pool: &rustango::sql::Pool) {
        let entries: &[(&str, &str)] = &[];
        let _fut = rustango::migrate::migrate_embedded_pool(pool, entries);
    }
}

#[test]
fn audit_mysql_ddl_uses_backticks_and_json() {
    // batch 16 — confirm the MySQL-shape DDL uses backticks +
    // JSON + DATETIME(6) (no JSONB / TIMESTAMPTZ / double quotes).
    let ddl = rustango::audit::CREATE_TABLE_SQL_MYSQL;
    assert!(ddl.contains("`rustango_audit_log`"));
    assert!(ddl.contains("`changes`      JSON NOT NULL"));
    assert!(ddl.contains("DATETIME(6)"));
    assert!(ddl.contains("BIGINT AUTO_INCREMENT"));
    assert!(!ddl.contains("JSONB"));
    assert!(!ddl.contains("TIMESTAMPTZ"));
    assert!(!ddl.contains("BIGSERIAL"));
}
