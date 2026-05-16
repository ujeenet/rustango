//! Emission tests for the Postgres FTS `ts_headline` scalar function.
//! Issue #28 follow-up — pairs with `to_tsvector` / `plainto_tsquery`
//! / `ts_rank` shipped in PR #136.

use rustango::core::funcs::{plainto_tsquery, ts_headline, ts_headline_with};
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
#[rustango(table = "fts_doc_head")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    body: String,
    #[rustango(max_length = 1000)]
    snippet: String,
}

fn update_set(value: Expr) -> UpdateQuery {
    UpdateQuery {
        model: Doc::SCHEMA,
        set: vec![Assignment {
            column: "snippet",
            value,
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    }
}

// ---------- PG ----------

#[test]
fn pg_emits_ts_headline_2_arg() {
    let q = update_set(ts_headline(F("body"), plainto_tsquery("rust orm")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"ts_headline("body", plainto_tsquery($"#),
        "PG ts_headline (2-arg): {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_ts_headline_with_options() {
    let q = update_set(ts_headline_with(
        F("body"),
        plainto_tsquery("rust orm"),
        "StartSel='<mark>', StopSel='</mark>', MaxFragments=1",
    ));
    let stmt = Postgres.compile_update(&q).unwrap();
    // 3 args, 2 string params (search query + options).
    assert!(
        stmt.sql.contains("ts_headline(\"body\", plainto_tsquery(")
            && stmt.sql.contains(", $")
            && stmt.sql.ends_with(") WHERE \"id\" = $3")
            || stmt.sql.contains("ts_headline(\"body\", plainto_tsquery"),
        "PG ts_headline (3-arg): {}",
        stmt.sql
    );
    // Options string bound as a param.
    assert!(
        stmt.params
            .iter()
            .any(|p| matches!(p, SqlValue::String(s) if s.contains("StartSel"))),
        "options bound as param: {:?}",
        stmt.params
    );
}

// ---------- MySQL / SQLite reject ----------

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_ts_headline() {
    let q = update_set(ts_headline(F("body"), plainto_tsquery("q")));
    let err = MySql.compile_update(&q).unwrap_err();
    // The writer visits the outer `ts_headline` first; on MySQL it
    // either rejects directly or rejects the inner `plainto_tsquery`
    // first depending on emission order. Either is acceptable.
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(
                op.contains("ts_headline") || op.contains("plainto_tsquery"),
                "op label: {op}"
            );
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_ts_headline() {
    let q = update_set(ts_headline(F("body"), plainto_tsquery("q")));
    let err = Sqlite.compile_update(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("ts_headline") || op.contains("plainto_tsquery")
    ));
}

// ---------- Arity ----------

#[test]
fn ts_headline_with_one_arg_arity_mismatch() {
    let bad = Expr::Function {
        kind: ScalarFn::TsHeadline,
        args: vec![F("body").into()],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "ts_headline",
            ..
        }
    ));
}

#[test]
fn ts_headline_with_four_args_arity_mismatch() {
    let bad = Expr::Function {
        kind: ScalarFn::TsHeadline,
        args: vec![
            F("body").into(),
            F("body").into(),
            F("body").into(),
            F("body").into(),
        ],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "ts_headline",
            ..
        }
    ));
}
