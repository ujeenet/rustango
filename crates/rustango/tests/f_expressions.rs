//! Tri-dialect emission tests for `F()` expressions (issue #1) — the
//! ORM Expression DSL primitive. Pins the SQL each backend produces
//! for the four shapes that matter:
//!
//! 1. **Column-vs-column WHERE compare** — `WHERE start < end`.
//! 2. **Column-vs-arithmetic WHERE compare** — `WHERE price > cost * 2`.
//! 3. **Atomic counter UPDATE** — `SET views = views + 1`.
//! 4. **Column-to-column UPDATE copy** — `SET full_name = display_name`.
//!
//! And the negative path: `BinOp::BitXor` is not emittable on SQLite
//! (the dialect has `&`, `|`, `<<`, `>>` but no XOR symbol) — the
//! writer surfaces a clear `OpNotSupportedInDialect` error rather than
//! silently emitting wrong SQL.

use rustango::core::{
    Assignment, BinOp, ColumnFilter, Expr, Filter, Model as _, Op, SqlValue, UpdateQuery,
    WhereExpr, F,
};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "counter")]
#[allow(dead_code)]
pub struct Counter {
    #[rustango(primary_key)]
    id: i64,
    views: i64,
    threshold: i64,
}

// ---------- Expr type-level tests ----------

#[test]
fn f_lifts_to_expr_column() {
    let e: Expr = F("views").into();
    assert_eq!(e, Expr::Column("views"));
}

#[test]
fn arithmetic_chains_left_assoc() {
    // `(views + 1) - 2` per Rust precedence.
    let e: Expr = F("views") + 1 - 2;
    let Expr::BinOp { left, op, right } = e else {
        panic!("expected outer BinOp")
    };
    assert_eq!(op, BinOp::Sub);
    assert_eq!(*right, Expr::Literal(SqlValue::I32(2)));
    let Expr::BinOp { op: inner, .. } = *left else {
        panic!()
    };
    assert_eq!(inner, BinOp::Add);
}

// ---------- UPDATE: atomic counter increment ----------

fn update_increment_views() -> UpdateQuery {
    UpdateQuery {
        model: Counter::SCHEMA,
        set: vec![Assignment {
            column: "views",
            value: (F("views") + 1_i64).into(),
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(7),
        }),
    }
}

#[test]
fn pg_emits_views_plus_one() {
    let stmt = Postgres.compile_update(&update_increment_views()).unwrap();
    assert_eq!(
        stmt.sql,
        r#"UPDATE "counter" SET "views" = ("views" + $1) WHERE "id" = $2"#
    );
    assert_eq!(stmt.params, vec![SqlValue::I64(1), SqlValue::I64(7)]);
}

#[test]
fn mysql_emits_views_plus_one_with_backticks_and_question_marks() {
    let stmt = MySql.compile_update(&update_increment_views()).unwrap();
    assert_eq!(
        stmt.sql,
        "UPDATE `counter` SET `views` = (`views` + ?) WHERE `id` = ?"
    );
    assert_eq!(stmt.params, vec![SqlValue::I64(1), SqlValue::I64(7)]);
}

#[test]
fn sqlite_emits_views_plus_one() {
    let stmt = Sqlite.compile_update(&update_increment_views()).unwrap();
    assert_eq!(
        stmt.sql,
        r#"UPDATE "counter" SET "views" = ("views" + ?) WHERE "id" = ?"#
    );
    assert_eq!(stmt.params, vec![SqlValue::I64(1), SqlValue::I64(7)]);
}

// ---------- UPDATE: column-to-column copy (no arithmetic) ----------

#[test]
fn pg_set_column_to_column_copies_without_param() {
    let q = UpdateQuery {
        model: Counter::SCHEMA,
        set: vec![Assignment {
            column: "views",
            value: F("threshold").into(),
        }],
        where_clause: WhereExpr::And(vec![]),
    };
    let stmt = Postgres.compile_update(&q).unwrap();
    assert_eq!(stmt.sql, r#"UPDATE "counter" SET "views" = "threshold""#);
    assert!(
        stmt.params.is_empty(),
        "column-to-column copy must not bind any params, got {:?}",
        stmt.params
    );
}

// ---------- WHERE: column-vs-column ----------

fn select_views_gt_threshold() -> rustango::core::SelectQuery {
    // Build the bulk of the SelectQuery via the QuerySet helper to
    // dodge ABI churn on `SelectQuery` field additions, then surgically
    // swap in the ColumnCompare WhereExpr.
    let mut q = Counter::objects().compile().unwrap();
    q.where_clause = WhereExpr::ColumnCompare(ColumnFilter {
        column: "views",
        op: Op::Gt,
        rhs: Expr::Column("threshold"),
    });
    q
}

#[test]
fn pg_where_column_gt_column() {
    let stmt = Postgres
        .compile_select(&select_views_gt_threshold())
        .unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "views" > "threshold""#),
        "missing column-vs-column predicate: {}",
        stmt.sql
    );
    assert!(stmt.params.is_empty());
}

#[test]
fn mysql_where_column_gt_column() {
    let stmt = MySql.compile_select(&select_views_gt_threshold()).unwrap();
    assert!(
        stmt.sql.contains("WHERE `views` > `threshold`"),
        "missing column-vs-column predicate: {}",
        stmt.sql
    );
    assert!(stmt.params.is_empty());
}

#[test]
fn sqlite_where_column_gt_column() {
    let stmt = Sqlite.compile_select(&select_views_gt_threshold()).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "views" > "threshold""#),
        "missing column-vs-column predicate: {}",
        stmt.sql
    );
    assert!(stmt.params.is_empty());
}

// ---------- WHERE: column-vs-arithmetic ----------

#[test]
fn pg_where_column_compared_to_arithmetic_expr() {
    let mut q = Counter::objects().compile().unwrap();
    q.where_clause = WhereExpr::ColumnCompare(ColumnFilter {
        column: "views",
        op: Op::Gte,
        rhs: F("threshold") * 2_i64,
    });
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "views" >= ("threshold" * $1)"#),
        "got: {}",
        stmt.sql
    );
    assert_eq!(stmt.params, vec![SqlValue::I64(2)]);
}

// ---------- Bitwise ops: PG `#`, MySQL `^`, SQLite errors out ----------

fn update_xor_threshold() -> UpdateQuery {
    UpdateQuery {
        model: Counter::SCHEMA,
        set: vec![Assignment {
            column: "views",
            value: (F("views") ^ 0xff_i32).into(),
        }],
        where_clause: WhereExpr::And(vec![]),
    }
}

#[test]
fn pg_bitxor_emits_hash_operator() {
    let stmt = Postgres.compile_update(&update_xor_threshold()).unwrap();
    assert!(
        stmt.sql.contains(r#""views" = ("views" # $1)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_bitxor_emits_caret_operator() {
    let stmt = MySql.compile_update(&update_xor_threshold()).unwrap();
    assert!(
        stmt.sql.contains("`views` = (`views` ^ ?)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_bitxor_returns_op_not_supported_error() {
    let err = Sqlite.compile_update(&update_xor_threshold()).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert_eq!(op, "BitXor");
            assert_eq!(dialect, "sqlite");
        }
        other => panic!("expected OpNotSupportedInDialect, got {other:?}"),
    }
}

// ---------- UpdateBuilder integration (the user-facing API) ----------

#[test]
fn update_builder_set_expr_resolves_field_name() {
    let q = Counter::objects()
        .eq("id", 7_i64)
        .update()
        .set_expr("views", F("views") + 1_i64)
        .compile()
        .unwrap();
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""views" = ("views" + $1)"#),
        "got: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains(r#""id" = $2"#), "got: {}", stmt.sql);
}

#[test]
fn update_builder_set_expr_rejects_unknown_field() {
    let err = Counter::objects()
        .update()
        .set_expr("nope_nope", F("views") + 1_i64)
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, rustango::core::QueryError::UnknownField { ref field, .. }
                 if field == "nope_nope"),
        "expected UnknownField for the set-target column, got {err:?}",
    );
}

#[test]
fn update_builder_set_expr_rejects_unknown_column_inside_expr() {
    let err = Counter::objects()
        .update()
        .set_expr("views", F("nope_nope") + 1_i64)
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, rustango::core::QueryError::UnknownField { ref field, .. }
                 if field == "nope_nope"),
        "expected UnknownField for the F() column inside the expr, got {err:?}",
    );
}
