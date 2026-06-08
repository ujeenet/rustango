//! Tri-dialect emission tests for `QuerySet::where_has_count(name, op, n)`
//! — issue #830 slice 3 (count-comparator `has($rel, $op, $n)`).
//!
//! The method resolves the named reverse-FK relation via
//! `Model::reverse_relations()` and embeds a correlated scalar-aggregate
//! subquery `(SELECT COUNT(*) FROM <child> WHERE <child_fk> =
//! <outer>.<pk>) <op> n` in the outer WHERE clause. The generated SQL is
//! standard across PG / MySQL / SQLite apart from identifier quoting and
//! the placeholder shape — these tests pin both.

use rustango::core::{Model as _, Op};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(
    table = "whc_author",
    reverse_has(name = "books", child = "Book", child_fk_column = "author_id",)
)]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 40)]
    name: String,
}

#[derive(Model)]
#[rustango(table = "whc_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    title: String,
    author_id: i64,
}

fn compiled_sql<D: Dialect>(d: &D, op: Op, n: i64) -> String {
    let q = Author::objects()
        .where_has_count("books", op, n)
        .compile()
        .expect("compile where_has_count");
    d.compile_select(&q).expect("emit SQL").sql
}

#[test]
fn pg_emits_correlated_count_subquery_with_dollar_placeholder() {
    let sql = compiled_sql(&Postgres, Op::Gt, 3);
    // Correlated `COUNT(*)` over the child table.
    assert!(sql.contains("COUNT(*)"), "missing COUNT(*): {sql}");
    assert!(
        sql.contains(r#"FROM "whc_book" WHERE "author_id" = "whc_author"."id""#),
        "missing correlated child WHERE: {sql}"
    );
    // Comparator + bound count, dollar placeholder on PG.
    assert!(sql.contains(") > $1"), "missing `) > $1`: {sql}");
}

#[test]
fn sqlite_emits_question_mark_placeholder() {
    let sql = compiled_sql(&Sqlite, Op::Gte, 1);
    assert!(sql.contains("COUNT(*)"), "missing COUNT(*): {sql}");
    assert!(
        sql.contains(r#"FROM "whc_book" WHERE "author_id" = "whc_author"."id""#),
        "missing correlated child WHERE: {sql}"
    );
    assert!(sql.contains(") >= ?"), "missing `) >= ?`: {sql}");
}

#[test]
fn mysql_emits_backtick_quoting_and_question_mark() {
    let sql = compiled_sql(&MySql, Op::Lt, 5);
    assert!(sql.contains("COUNT(*)"), "missing COUNT(*): {sql}");
    assert!(
        sql.contains("FROM `whc_book` WHERE `author_id` = `whc_author`.`id`"),
        "missing correlated child WHERE: {sql}"
    );
    assert!(sql.contains(") < ?"), "missing `) < ?`: {sql}");
}

#[test]
fn each_comparison_operator_emits_its_symbol() {
    for (op, sym) in [
        (Op::Eq, " = "),
        (Op::Ne, " <> "),
        (Op::Lt, " < "),
        (Op::Lte, " <= "),
        (Op::Gt, " > "),
        (Op::Gte, " >= "),
    ] {
        let sql = compiled_sql(&Postgres, op, 2);
        assert!(
            sql.contains(&format!("){sym}$1")),
            "op {op:?} expected `){sym}$1` in: {sql}"
        );
    }
}

#[test]
fn unknown_relation_errors_at_compile_time() {
    let err = Author::objects()
        .where_has_count("nope", Op::Gt, 1)
        .compile()
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("nope"),
        "expected UnknownField naming the bad relation, got: {msg}"
    );
}

#[test]
fn reverse_relations_metadata_drives_resolution() {
    let rels = Author::reverse_relations();
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].name, "books");
    assert_eq!(rels[0].child_schema.table, "whc_book");
    assert_eq!(rels[0].child_fk_column, "author_id");
    assert_eq!(rels[0].self_pk_column, "id");
}
