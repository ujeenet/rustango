//! Tri-dialect emission tests for `CASE WHEN … THEN … ELSE … END`
//! conditional expressions (issue #4). The SQL is standard SQL-92 —
//! identical across PG / MySQL / SQLite — so most tests just confirm
//! the writer emits the same string for every backend and the
//! placeholder formatting matches the dialect.

use rustango::core::{
    case::{case, value},
    funcs::lower,
    Assignment, Column as _, Expr, Filter, Model as _, Op, SqlValue, UpdateQuery, WhereExpr, F,
};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "csq")]
#[allow(dead_code)]
pub struct Csq {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 20)]
    status: String,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 50)]
    slug: String,
    score: i64,
}

fn update_set(value: Expr) -> UpdateQuery {
    UpdateQuery {
        model: Csq::SCHEMA,
        set: vec![Assignment {
            column: "title",
            value,
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    }
}

// ---------- Emit shape: single WHEN + ELSE ----------

#[test]
fn pg_emits_standard_case_when_then_else_end() {
    let expr: Expr = case()
        .when(Csq::status.eq("draft"), value("Draft Title"))
        .default(value("Published Title"))
        .into();
    let stmt = Postgres.compile_update(&update_set(expr)).unwrap();
    assert!(
        stmt.sql
            .contains(r#"= CASE WHEN "status" = $1 THEN $2 ELSE $3 END"#),
        "got: {}",
        stmt.sql
    );
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::String("draft".into()),
            SqlValue::String("Draft Title".into()),
            SqlValue::String("Published Title".into()),
            SqlValue::I64(1),
        ],
    );
}

#[test]
fn mysql_emits_same_shape_with_backticks_and_question_marks() {
    let expr: Expr = case()
        .when(Csq::status.eq("draft"), value("Draft Title"))
        .default(value("Published Title"))
        .into();
    let stmt = MySql.compile_update(&update_set(expr)).unwrap();
    assert!(
        stmt.sql
            .contains("= CASE WHEN `status` = ? THEN ? ELSE ? END"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_emits_same_shape_with_double_quotes() {
    let expr: Expr = case()
        .when(Csq::status.eq("draft"), value("Draft Title"))
        .default(value("Published Title"))
        .into();
    let stmt = Sqlite.compile_update(&update_set(expr)).unwrap();
    assert!(
        stmt.sql
            .contains(r#"= CASE WHEN "status" = ? THEN ? ELSE ? END"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Multiple branches preserve source order ----------

#[test]
fn multiple_when_branches_emit_in_chained_order() {
    let expr: Expr = case()
        .when(Csq::status.eq("draft"), 1_i64)
        .when(Csq::status.eq("review"), 2_i64)
        .when(Csq::status.eq("published"), 3_i64)
        .default(0_i64)
        .into();
    let stmt = Postgres.compile_update(&update_set(expr)).unwrap();
    // Each WHEN ... THEN appears once, in chain order.
    let positions: Vec<_> = ["draft", "review", "published"]
        .iter()
        .map(|s| {
            stmt.params
                .iter()
                .position(|p| matches!(p, SqlValue::String(v) if v == s))
        })
        .collect();
    assert_eq!(
        positions,
        vec![Some(0), Some(2), Some(4)],
        "WHEN-value placeholders should appear in chain order: {:?}",
        stmt.params
    );
}

// ---------- ELSE is optional ----------

#[test]
fn no_default_omits_else_clause() {
    let expr: Expr = case().when(Csq::status.eq("draft"), value("Draft")).into();
    let stmt = Postgres.compile_update(&update_set(expr)).unwrap();
    assert!(stmt.sql.contains("CASE WHEN"), "got: {}", stmt.sql);
    assert!(stmt.sql.contains("THEN"), "got: {}", stmt.sql);
    assert!(stmt.sql.contains("END"), "got: {}", stmt.sql);
    assert!(
        !stmt.sql.contains("ELSE"),
        "no-default case should not emit ELSE: {}",
        stmt.sql
    );
}

// ---------- Composition: nested CASE, CASE-in-function, function-in-CASE ----------

#[test]
fn case_composes_with_function_call_in_then_branch() {
    // CASE WHEN status='draft' THEN LOWER(title) ELSE title END
    let expr: Expr = case()
        .when(Csq::status.eq("draft"), lower(F("title")))
        .default(F("title"))
        .into();
    let stmt = Postgres.compile_update(&update_set(expr)).unwrap();
    assert!(
        stmt.sql
            .contains(r#"CASE WHEN "status" = $1 THEN LOWER("title") ELSE "title" END"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn function_composes_with_case_as_argument() {
    // LOWER(CASE WHEN status='draft' THEN 'Draft' ELSE 'Pub' END)
    let inner = case()
        .when(Csq::status.eq("draft"), value("Draft"))
        .default(value("Pub"))
        .build();
    let expr = lower(inner);
    let stmt = Postgres.compile_update(&update_set(expr)).unwrap();
    assert!(
        stmt.sql.contains("LOWER(CASE WHEN"),
        "case-in-function should wrap: {}",
        stmt.sql
    );
}

#[test]
fn nested_case_renders_recursively() {
    // CASE WHEN status='draft' THEN
    //     CASE WHEN score>10 THEN 'good draft' ELSE 'bad draft' END
    // ELSE 'published' END
    let inner = case()
        .when(Csq::score.gt(10_i64), value("good draft"))
        .default(value("bad draft"))
        .build();
    let outer = case()
        .when(Csq::status.eq("draft"), inner)
        .default(value("published"))
        .into();
    let stmt = Postgres.compile_update(&update_set(outer)).unwrap();
    // Two CASE keywords + two END keywords appear when nested.
    assert_eq!(stmt.sql.matches("CASE WHEN").count(), 2);
    assert_eq!(stmt.sql.matches(" END").count(), 2);
}

// ---------- AND/OR conditions in WHEN ----------

#[test]
fn when_condition_accepts_and_or_typed_expr() {
    // WHEN (status='draft' AND score>10) OR (status='review')
    let cond = Csq::status
        .eq("draft")
        .and(Csq::score.gt(10_i64))
        .or(Csq::status.eq("review"));
    let expr: Expr = case()
        .when(cond, value("hot"))
        .default(value("cold"))
        .into();
    let stmt = Postgres.compile_update(&update_set(expr)).unwrap();
    // The exact paren shape comes from the WhereExpr writer; we just
    // confirm the keywords made it through.
    let s = &stmt.sql;
    assert!(s.contains("CASE WHEN"));
    assert!(s.contains("AND"));
    assert!(s.contains("OR"));
    assert!(s.contains("THEN $4"), "got: {}", s);
}

// ---------- Errors: empty branches ----------

#[test]
fn empty_branches_is_rejected_at_emit_time() {
    // Builder allows the empty state; writer rejects.
    let q = update_set(case().into());
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(
        matches!(err, SqlError::EmptyCaseBranches),
        "expected EmptyCaseBranches, got {err:?}",
    );
}

// ---------- F() column-ref inside THEN ----------

#[test]
fn then_branch_can_be_column_ref() {
    // SET title = CASE WHEN status='draft' THEN title ELSE slug END
    // — both branches are columns, no params bound.
    let expr: Expr = case()
        .when(Csq::status.eq("draft"), F("title"))
        .default(F("slug"))
        .into();
    let stmt = Postgres.compile_update(&update_set(expr)).unwrap();
    assert!(
        stmt.sql
            .contains(r#"CASE WHEN "status" = $1 THEN "title" ELSE "slug" END"#),
        "got: {}",
        stmt.sql
    );
    // Two params: the "draft" literal in WHERE-side check, plus the
    // WHERE id=1 literal — but no params for the columns themselves.
    assert_eq!(stmt.params.len(), 2);
}

// ---------- UpdateBuilder::set_expr schema-validates column refs ----------

#[test]
fn unknown_column_inside_case_when_predicate_is_caught() {
    let err = Csq::objects()
        .update()
        .set_expr(
            "title",
            case()
                .when(
                    // bogus column in the predicate
                    WhereExpr::Predicate(Filter {
                        column: "nope_col",
                        op: Op::Eq,
                        value: SqlValue::I64(1),
                    }),
                    value("anything"),
                )
                .default(value("fallback")),
        )
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, rustango::core::QueryError::UnknownField { ref field, .. }
                 if field == "nope_col"),
        "expected UnknownField for the WHEN-predicate column, got: {err:?}",
    );
}

#[test]
fn unknown_column_inside_case_then_branch_is_caught() {
    let err = Csq::objects()
        .update()
        .set_expr(
            "title",
            case()
                .when(Csq::status.eq("draft"), F("nope_then"))
                .default(value("fallback")),
        )
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, rustango::core::QueryError::UnknownField { ref field, .. }
                 if field == "nope_then"),
        "expected UnknownField for the THEN F() ref, got: {err:?}",
    );
}

#[test]
fn unknown_column_inside_case_default_is_caught() {
    let err = Csq::objects()
        .update()
        .set_expr(
            "title",
            case()
                .when(Csq::status.eq("draft"), value("ok"))
                .default(F("nope_default")),
        )
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, rustango::core::QueryError::UnknownField { ref field, .. }
                 if field == "nope_default"),
        "expected UnknownField for the default F() ref, got: {err:?}",
    );
}
