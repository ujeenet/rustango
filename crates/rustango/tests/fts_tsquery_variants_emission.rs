//! Emission tests for the Postgres FTS `tsquery` parser family +
//! `ts_rank_cd`. Issue #28 follow-up — fills out the FTS scalar-
//! function surface alongside `to_tsvector` / `plainto_tsquery` /
//! `ts_rank` (PR #136) and `ts_headline` (PR #138).

use rustango::core::funcs::{
    phraseto_tsquery, to_tsquery, to_tsvector, ts_rank_cd, websearch_to_tsquery,
};
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
#[rustango(table = "fts_doc_tsquery")]
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

// ---------- PG emission ----------

#[test]
fn pg_emits_phraseto_tsquery() {
    let q = update_set(phraseto_tsquery("rust orm"));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("phraseto_tsquery($"),
        "PG phraseto_tsquery: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_websearch_to_tsquery() {
    let q = update_set(websearch_to_tsquery("\"rust orm\" -python"));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("websearch_to_tsquery($"),
        "PG websearch_to_tsquery: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_to_tsquery() {
    let q = update_set(to_tsquery("rust & orm"));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("to_tsquery($"),
        "PG to_tsquery: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_ts_rank_cd_composed() {
    let q = update_set(ts_rank_cd(
        to_tsvector(F("body")),
        phraseto_tsquery("rust orm"),
    ));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains("ts_rank_cd(to_tsvector(\"body\"), phraseto_tsquery($"),
        "PG ts_rank_cd composed: {}",
        stmt.sql
    );
}

// ---------- MySQL / SQLite reject ----------

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_phraseto_tsquery() {
    let err = MySql
        .compile_update(&update_set(phraseto_tsquery("q")))
        .unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("phraseto_tsquery")
    ));
}

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_websearch_to_tsquery() {
    let err = MySql
        .compile_update(&update_set(websearch_to_tsquery("q")))
        .unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("websearch_to_tsquery")
    ));
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_to_tsquery() {
    let err = Sqlite
        .compile_update(&update_set(to_tsquery("q")))
        .unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("to_tsquery")
    ));
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_ts_rank_cd() {
    let err = Sqlite
        .compile_update(&update_set(ts_rank_cd(
            to_tsvector(F("body")),
            phraseto_tsquery("q"),
        )))
        .unwrap_err();
    // Inside-out evaluation: to_tsvector reaches first.
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. }
            if op.contains("to_tsvector") || op.contains("ts_rank_cd")
    ));
}

// ---------- Arity ----------

#[test]
fn phraseto_tsquery_with_two_args_arity_error() {
    let bad = Expr::Function {
        kind: ScalarFn::PhraseToTsQuery,
        args: vec![F("body").into(), F("body").into()],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "phraseto_tsquery",
            ..
        }
    ));
}

#[test]
fn ts_rank_cd_with_one_arg_arity_error() {
    let bad = Expr::Function {
        kind: ScalarFn::TsRankCd,
        args: vec![F("body").into()],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "ts_rank_cd",
            ..
        }
    ));
}
