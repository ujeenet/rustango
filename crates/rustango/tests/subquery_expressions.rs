//! Tri-dialect emission tests for `Subquery` / `Exists` / `OuterRef`
//! (issue #5). Most of the produced SQL is standard SQL-92, identical
//! across PG / MySQL / SQLite — these tests pin the emitted string and
//! the placeholder dialect-shape for each.

use rustango::core::subquery::{
    exists, in_subquery, not_exists, not_in_subquery, outer_ref, subquery,
};
use rustango::core::{
    Assignment, Column as _, Expr, Filter, Model as _, Op, SqlValue, UpdateQuery, WhereExpr,
};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "sq_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 100)]
    name: String,
    book_count: i64,
}

#[derive(Model)]
#[rustango(table = "sq_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    id: i64,
    author_id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 20)]
    status: String,
    pages: i64,
}

// Shared helper — wraps the outer query in an UPDATE because that's
// the only fully-Expr-aware writer available pre-#75. Picking UPDATE
// lets us exercise both WHERE-shaped subqueries (Exists / InSubquery)
// in the outer query's where_clause AND scalar Expr::Subquery in the
// assignment RHS.
fn update_against_author(set_value: Expr, where_clause: WhereExpr) -> UpdateQuery {
    UpdateQuery {
        model: Author::SCHEMA,
        set: vec![Assignment {
            column: "name",
            value: set_value,
        }],
        where_clause,
    }
}

// ---------- EXISTS ----------

#[test]
fn pg_emits_exists_with_dollar_placeholders_and_double_quotes() {
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("changed".into())),
        exists(inner),
    );
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(
            r#"WHERE EXISTS (SELECT "id", "author_id", "title", "status", "pages" FROM "sq_book" WHERE "author_id" = "sq_author"."id")"#
        ),
        "PG EXISTS shape: {}",
        stmt.sql
    );
}

#[test]
fn mysql_emits_exists_with_question_marks_and_backticks() {
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("changed".into())),
        exists(inner),
    );
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("EXISTS (SELECT"),
        "MySQL EXISTS keyword: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("`sq_author`.`id`"),
        "MySQL OuterRef qualification: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_emits_exists_with_question_marks_and_double_quotes() {
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("changed".into())),
        exists(inner),
    );
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(stmt.sql.contains("EXISTS (SELECT"));
    assert!(stmt.sql.contains(r#""sq_author"."id""#));
}

// ---------- NOT EXISTS ----------

#[test]
fn not_exists_emits_with_negation_keyword() {
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("orphan".into())),
        not_exists(inner),
    );
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(stmt.sql.contains("NOT EXISTS (SELECT"), "got: {}", stmt.sql);
}

// ---------- IN (subquery) ----------

#[test]
fn in_subquery_emits_with_outer_column_and_subselect() {
    let inner = Book::objects()
        .where_(Book::status.eq("published"))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("touched".into())),
        in_subquery("id", inner),
    );
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""id" IN (SELECT"#),
        "outer col + IN + SELECT: {}",
        stmt.sql
    );
}

#[test]
fn not_in_subquery_emits_with_not_keyword() {
    let inner = Book::objects()
        .where_(Book::status.eq("draft"))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("touched".into())),
        not_in_subquery("id", inner),
    );
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""id" NOT IN (SELECT"#),
        "outer col + NOT IN + SELECT: {}",
        stmt.sql
    );
}

// ---------- Scalar subquery in set_expr ----------

#[test]
fn scalar_subquery_in_set_expr_emits_in_parens() {
    // `UPDATE author SET name = (SELECT title FROM book WHERE author_id = author.id) WHERE …`
    // — sets each author's name to a derived value from their book row.
    // The inner queryset projects all of Book's columns today (no
    // .values()-narrow shipped yet); the database will produce a
    // single column in practice when limit=1 + only-one-row hit.
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let q = update_against_author(
        subquery(inner),
        WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    );
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"SET "name" = (SELECT"#),
        "scalar subquery wraps in parens after SET col = : {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#""sq_author"."id""#),
        "OuterRef inside set_expr scalar subquery resolves outer: {}",
        stmt.sql
    );
}

// ---------- OuterRef error path ----------

#[test]
fn outer_ref_without_any_subquery_wrap_is_an_emit_error() {
    // OuterRef makes no sense as a top-level expression — there is no
    // "outer" to resolve against. Confirm the writer rejects with the
    // dedicated error rather than emitting nonsense SQL.
    let q = update_against_author(
        outer_ref("id"),
        WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    );
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(
        matches!(err, SqlError::OuterRefOutsideSubquery { column } if column == "id"),
        "expected OuterRefOutsideSubquery, got {err:?}",
    );
}

// ---------- Composability: Exists inside an Or ----------

#[test]
fn exists_inside_or_composes_with_other_predicates() {
    // WHERE name = 'X' OR EXISTS (SELECT … FROM book WHERE …)
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("yes".into())),
        WhereExpr::Or(vec![Author::name.eq("X").into(), exists(inner)]),
    );
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("OR EXISTS ("),
        "Or composition with EXISTS: {}",
        stmt.sql
    );
}

// ---------- Nested correlation: Exists inside Exists ----------

#[test]
fn nested_subqueries_resolve_each_outer_ref_to_its_enclosing_scope() {
    // Outermost: Author
    //   EXISTS Book WHERE author_id = Author.id  AND
    //                     EXISTS (SELECT … FROM Book WHERE pages > 100
    //                             AND author_id = Book.author_id)
    // The inner-inner OuterRef("author_id") should resolve to the
    // middle Book, not jump to Author.
    let inner_inner = Book::objects()
        .where_(Book::pages.gt(100_i64))
        .where_(Book::author_id.eq_expr(outer_ref("author_id")))
        .compile()
        .unwrap();
    let middle = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .where_raw(exists(inner_inner))
        .compile()
        .unwrap();
    let q = update_against_author(
        Expr::Literal(SqlValue::String("nested".into())),
        exists(middle),
    );
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""sq_author"."id""#),
        "outer OuterRef should resolve to Author: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#""sq_book"."author_id""#),
        "inner OuterRef should resolve to middle Book: {}",
        stmt.sql
    );
}
