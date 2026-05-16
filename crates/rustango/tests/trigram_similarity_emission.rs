//! Emission tests for `SIMILARITY` / `WORD_SIMILARITY` pg_trgm
//! annotation functions (issue #29 follow-up). PG emits the
//! native calls; MySQL and SQLite reject with
//! `OpNotSupportedInDialect`.
//!
//! Pairs with the `__trigram_similar` / `__trigram_word_similar`
//! WHERE-clause lookups (already shipped).

use rustango::core::funcs::{trigram_similarity, trigram_word_similarity};
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
#[rustango(table = "trg_sim_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
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

// ---------- PG: native SIMILARITY / WORD_SIMILARITY ----------

#[test]
fn pg_emits_similarity_call() {
    let q = update_set(trigram_similarity(F("title"), "rust orm"));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"SIMILARITY("title", $1)"#),
        "PG SIMILARITY: {}",
        stmt.sql
    );
    // Pattern bound as the only string param.
    assert!(
        stmt.params
            .iter()
            .any(|p| matches!(p, SqlValue::String(s) if s == "rust orm")),
        "pattern bound as param: {:?}",
        stmt.params
    );
}

#[test]
fn pg_emits_word_similarity_call() {
    let q = update_set(trigram_word_similarity(F("title"), "rust"));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WORD_SIMILARITY("title", $1)"#),
        "PG WORD_SIMILARITY: {}",
        stmt.sql
    );
}

// ---------- MySQL / SQLite: rejected at compile time ----------

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_similarity_with_clean_error() {
    let q = update_set(trigram_similarity(F("title"), "rust"));
    let err = MySql.compile_update(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("SIMILARITY"), "op label: {op}");
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_word_similarity_with_clean_error() {
    let q = update_set(trigram_word_similarity(F("title"), "rust"));
    let err = Sqlite.compile_update(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("WORD_SIMILARITY"), "op label: {op}");
            assert_eq!(dialect, "sqlite");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

// ---------- Arity check (writer-level) ----------

#[test]
fn similarity_with_wrong_arity_returns_function_arity_mismatch() {
    // Hand-build an Expr with arity 1 to bypass the helper fn's
    // type-locked 2-arg signature. The writer should surface
    // FunctionArityMismatch.
    let bad = Expr::Function {
        kind: ScalarFn::TrigramSimilarity,
        args: vec![F("title").into()],
    };
    let q = update_set(bad);
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::FunctionArityMismatch {
                func: "SIMILARITY",
                ..
            }
        ),
        "expected FunctionArityMismatch{{ func: \"SIMILARITY\" }}, got: {err:?}"
    );
}

#[test]
fn word_similarity_with_three_args_arity_error() {
    let bad = Expr::Function {
        kind: ScalarFn::TrigramWordSimilarity,
        args: vec![F("title").into(), F("title").into(), F("title").into()],
    };
    let q = update_set(bad);
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::FunctionArityMismatch {
                func: "WORD_SIMILARITY",
                ..
            }
        ),
        "expected FunctionArityMismatch{{ func: \"WORD_SIMILARITY\" }}, got: {err:?}"
    );
}
