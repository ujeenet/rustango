//! Emission tests for Postgres FTS scalar functions
//! (`to_tsvector`, `plainto_tsquery`, `ts_rank`) — issue #28
//! follow-up. PG emits the native calls; MySQL and SQLite reject
//! at compile time with `OpNotSupportedInDialect`.
//!
//! Pairs with the `__search` WHERE-clause lookup shipped in PR #131.

use rustango::core::funcs::{plainto_tsquery, to_tsvector, ts_rank};
use rustango::core::{
    Assignment, Expr, Filter, Model as _, Op, ScalarFn, SqlValue, UpdateQuery, WhereExpr, F,
};
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres, SqlError};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "fts_doc_rank")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    body: String,
    rank: f64,
}

fn update_set(value: Expr) -> UpdateQuery {
    UpdateQuery {
        model: Doc::SCHEMA,
        set: vec![Assignment {
            column: "rank",
            value,
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    }
}

// ---------- PG: native emission ----------

#[test]
fn pg_emits_to_tsvector() {
    let q = update_set(to_tsvector(F("body")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"to_tsvector("body")"#),
        "PG to_tsvector: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_plainto_tsquery() {
    let q = update_set(plainto_tsquery("rust orm"));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("plainto_tsquery($"),
        "PG plainto_tsquery: {}",
        stmt.sql
    );
    assert!(
        stmt.params
            .iter()
            .any(|p| matches!(p, SqlValue::String(s) if s == "rust orm")),
        "search query bound as param: {:?}",
        stmt.params
    );
}

#[test]
fn pg_emits_ts_rank_with_composed_subexprs() {
    let q = update_set(ts_rank(to_tsvector(F("body")), plainto_tsquery("rust orm")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"ts_rank(to_tsvector("body"), plainto_tsquery($"#),
        "PG ts_rank composed: {}",
        stmt.sql
    );
}

// ---------- MySQL / SQLite rejection ----------

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_to_tsvector() {
    let q = update_set(to_tsvector(F("body")));
    let err = MySql.compile_update(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("to_tsvector"), "op label: {op}");
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_plainto_tsquery() {
    let q = update_set(plainto_tsquery("q"));
    let err = MySql.compile_update(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("plainto_tsquery")
    ));
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_ts_rank() {
    let q = update_set(ts_rank(to_tsvector(F("body")), plainto_tsquery("q")));
    let err = Sqlite.compile_update(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            // The outer expression evaluates inside-out: `to_tsvector`
            // is reached first and rejected; the user retargets the
            // backend or replaces the call with a raw FTS5 expression.
            assert!(
                op.contains("to_tsvector") || op.contains("ts_rank"),
                "op label: {op}"
            );
            assert_eq!(dialect, "sqlite");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

// ---------- Arity checks ----------

#[test]
fn ts_rank_with_one_arg_arity_mismatch() {
    let bad = Expr::Function {
        kind: ScalarFn::TsRank,
        args: vec![F("body").into()],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "ts_rank",
            ..
        }
    ));
}

#[test]
fn to_tsvector_with_two_args_arity_mismatch() {
    let bad = Expr::Function {
        kind: ScalarFn::ToTsVector,
        args: vec![F("body").into(), F("body").into()],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "to_tsvector",
            ..
        }
    ));
}
