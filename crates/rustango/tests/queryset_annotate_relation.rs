//! Tri-dialect emission tests for the relation eager-aggregate family
//! `QuerySet::annotate_count` / `annotate_sum` / `annotate_avg` /
//! `annotate_max` / `annotate_min` / `annotate_exists` — issue #830
//! slice 4/5 (`withCount`/`withSum`/`withExists`/… by relation name).
//!
//! Each method resolves the named reverse-FK relation via
//! `Model::reverse_relations()` and projects a **correlated** aggregate
//! subquery — `(SELECT COUNT(*) FROM <child> WHERE <child_fk> =
//! <outer>.<pk>) AS <name>_count` — alongside the parent's scalar
//! columns (Django Shape 3: GROUP BY every parent column). Because the
//! aggregate comes from a correlated subquery rather than a JOIN it
//! never double-counts. The generated SQL is standard across
//! PG / MySQL / SQLite apart from identifier quoting; these tests pin
//! the projection shape, the auto-named alias, and the GROUP BY.

use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(
    table = "arc_author",
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
#[rustango(table = "arc_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    title: String,
    author_id: i64,
    pages: i64,
}

fn count_sql<D: Dialect>(d: &D) -> String {
    let q = Author::objects()
        .annotate_count("books")
        .compile()
        .expect("compile annotate_count");
    d.compile_aggregate(&q).expect("emit SQL").sql
}

#[test]
fn pg_annotate_count_projects_correlated_count_alongside_parent_columns() {
    let sql = count_sql(&Postgres);
    // Parent columns are projected (Shape 3) ...
    assert!(
        sql.contains(r#"SELECT "id", "name""#),
        "missing parent columns in projection: {sql}"
    );
    // ... plus the correlated count under the auto-named alias. The
    // inner `AS "c"` is the inert subquery-internal alias (the outer
    // context reads the scalar value, not the name).
    assert!(
        sql.contains(
            r#"(SELECT COUNT(*) AS "c" FROM "arc_book" WHERE "author_id" = "arc_author"."id") AS "books_count""#
        ),
        "missing correlated count projection: {sql}"
    );
    assert!(
        sql.contains(r#"FROM "arc_author" GROUP BY "id", "name""#),
        "missing GROUP BY over parent scalar columns: {sql}"
    );
}

#[test]
fn sqlite_annotate_count_uses_question_mark_quoting() {
    let sql = count_sql(&Sqlite);
    assert!(sql.contains("COUNT(*)"), "missing COUNT(*): {sql}");
    assert!(
        sql.contains(r#"(SELECT COUNT(*) AS "c" FROM "arc_book" WHERE "author_id" = "arc_author"."id") AS "books_count""#),
        "missing correlated count projection: {sql}"
    );
    assert!(
        sql.contains(r#"GROUP BY "id", "name""#),
        "missing GROUP BY: {sql}"
    );
}

#[test]
fn mysql_annotate_count_uses_backtick_quoting() {
    let sql = count_sql(&MySql);
    assert!(
        sql.contains(
            "(SELECT COUNT(*) AS `c` FROM `arc_book` WHERE `author_id` = `arc_author`.`id`) AS `books_count`"
        ),
        "missing backtick-quoted correlated count: {sql}"
    );
    assert!(
        sql.contains("GROUP BY `id`, `name`"),
        "missing GROUP BY: {sql}"
    );
}

#[test]
fn annotate_sum_names_alias_from_relation_and_column() {
    let q = Author::objects()
        .annotate_sum("books", "pages")
        .compile()
        .expect("compile annotate_sum");
    let sql = Postgres.compile_aggregate(&q).expect("emit SQL").sql;
    assert!(sql.contains("SUM("), "missing SUM(): {sql}");
    assert!(
        sql.contains(r#"AS "books_sum_pages""#),
        "alias should be `<rel>_sum_<col>`: {sql}"
    );
    // The aggregated column lives on the child table inside the subquery.
    assert!(
        sql.contains(r#"FROM "arc_book" WHERE "author_id" = "arc_author"."id""#),
        "missing correlated child WHERE: {sql}"
    );
}

#[test]
fn avg_max_min_use_their_suffix_and_function() {
    for (build, suffix, func) in [
        (
            Author::objects().annotate_avg("books", "pages"),
            "books_avg_pages",
            "AVG(",
        ),
        (
            Author::objects().annotate_max("books", "pages"),
            "books_max_pages",
            "MAX(",
        ),
        (
            Author::objects().annotate_min("books", "pages"),
            "books_min_pages",
            "MIN(",
        ),
    ] {
        let q = build.compile().expect("compile");
        let sql = Postgres.compile_aggregate(&q).expect("emit SQL").sql;
        assert!(sql.contains(func), "expected {func} in: {sql}");
        assert!(
            sql.contains(&format!(r#"AS "{suffix}""#)),
            "expected alias {suffix} in: {sql}"
        );
    }
}

#[test]
fn annotate_exists_emits_case_when_exists_with_integer_literals() {
    // PG — `CASE WHEN EXISTS (...) THEN 1 ELSE 0 END AS "books_exists"`.
    let q = Author::objects()
        .annotate_exists("books")
        .compile()
        .expect("compile annotate_exists");
    let sql = Postgres.compile_aggregate(&q).expect("emit SQL").sql;
    assert!(
        sql.contains(r#"CASE WHEN EXISTS (SELECT"#),
        "missing CASE WHEN EXISTS: {sql}"
    );
    assert!(
        sql.contains(r#"FROM "arc_book" WHERE "author_id" = "arc_author"."id""#),
        "missing correlated child WHERE: {sql}"
    );
    // Integer literals (bound params), not a native boolean.
    assert!(
        sql.contains("THEN $1 ELSE $2 END AS \"books_exists\""),
        "expected integer-literal CASE result aliased books_exists: {sql}"
    );

    // MySQL emits the same shape with backtick quoting.
    let my = MySql.compile_aggregate(&q).expect("emit MySQL").sql;
    assert!(
        my.contains("CASE WHEN EXISTS (SELECT") && my.contains("END AS `books_exists`"),
        "MySQL CASE/alias shape: {my}"
    );
}

#[test]
fn unknown_relation_errors_at_compile_time() {
    let err = Author::objects()
        .annotate_count("nope")
        .compile()
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("nope"),
        "expected UnknownField naming the bad relation, got: {msg}"
    );
}

#[test]
fn unknown_child_column_errors_at_compile_time() {
    let err = Author::objects()
        .annotate_sum("books", "no_such_col")
        .compile()
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("no_such_col"),
        "expected UnknownField naming the bad child column, got: {msg}"
    );
}

#[test]
fn annotate_count_composes_with_a_where_filter() {
    // The outer WHERE still applies; the correlated count is unaffected.
    let q = Author::objects()
        .filter("name", "Ada")
        .annotate_count("books")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_aggregate(&q).expect("emit SQL").sql;
    assert!(
        sql.contains(r#""name" = $1"#),
        "missing outer filter: {sql}"
    );
    assert!(
        sql.contains(r#"AS "books_count""#),
        "missing count projection: {sql}"
    );
}

// ---- scalar Subquery as a projected annotation (#1036) ----

#[test]
fn pg_scalar_subquery_annotation_projects_correlated_select() {
    use rustango::core::subquery::{outer_ref, scalar_subquery};
    use rustango::core::Column as _;
    // Newest book title per author — `annotate(newest=Subquery(...))`.
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .order_by(&[("id", true)])
        .limit(1)
        .values_list_flat("title")
        .compile()
        .expect("inner compile");
    let q = Author::objects()
        .annotate("newest", scalar_subquery(inner))
        .compile()
        .expect("compile");
    let sql = Postgres.compile_aggregate(&q).expect("emit SQL").sql;
    // Correlated scalar subquery, OuterRef rewritten to the parent
    // qualifier, projected under the alias.
    assert!(
        sql.contains(r#"(SELECT "title" FROM "arc_book" WHERE "author_id" = "arc_author"."id""#),
        "correlated scalar subquery projection: {sql}"
    );
    assert!(
        sql.contains(r#"LIMIT 1) AS "newest""#),
        "limited + aliased: {sql}"
    );
}
