//! Emission tests for `QuerySet::defer` / `QuerySet::only` (issue #20).
//! Both lower to the same projection IR as `.values_dict`, so the SQL
//! shape is identical — these tests pin the column-list semantics:
//! `.only(&[a, b])` selects exactly a + b, and `.defer(&[a])` selects
//! everything except a.

use rustango::core::QueryError;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "do_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 64)]
    title: String,
    /// Imagine a TEXT column we want to defer on list views.
    body: String,
    view_count: i64,
}

#[test]
fn only_emits_select_with_just_listed_cols() {
    let q = Post::objects().only(&["id", "title"]).compile().unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "id", "title" FROM "do_post""#),
        "only: {}",
        stmt.sql
    );
    // body and view_count must NOT appear.
    assert!(!stmt.sql.contains(r#""body""#), "{}", stmt.sql);
    assert!(!stmt.sql.contains(r#""view_count""#), "{}", stmt.sql);
}

#[test]
fn defer_emits_select_with_all_cols_minus_excluded() {
    let q = Post::objects().defer(&["body"]).compile().unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // Surviving columns: id, title, view_count — in model declaration order.
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "id", "title", "view_count" FROM "do_post""#),
        "defer: {}",
        stmt.sql
    );
    assert!(!stmt.sql.contains(r#""body""#), "{}", stmt.sql);
}

#[test]
fn defer_multiple_cols_excludes_all_named() {
    let q = Post::objects()
        .defer(&["body", "view_count"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "id", "title" FROM "do_post""#),
        "defer multi: {}",
        stmt.sql
    );
}

#[test]
fn defer_empty_list_returns_all_columns() {
    // Edge case: `.defer(&[])` is a semantic no-op — every column
    // survives. Matches Django's behavior.
    let q = Post::objects().defer(&[]).compile().unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "id", "title", "body", "view_count" FROM "do_post""#),
        "defer(empty): {}",
        stmt.sql
    );
}

#[test]
fn only_preserves_where_clause() {
    use rustango::core::Column as _;
    let q = Post::objects()
        .where_(Post::view_count.gt(100))
        .only(&["id", "title"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "view_count" > $1"#),
        "where survives: {}",
        stmt.sql
    );
}

#[test]
fn only_with_typo_errors_at_compile() {
    let err = Post::objects().only(&["id", "nope"]).compile().unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope"),
        "got: {err:?}"
    );
}

#[test]
fn defer_with_typo_errors_at_compile() {
    // A typo'd defer column shouldn't silently project all cols —
    // surface UnknownField so the caller learns about the typo.
    let err = Post::objects().defer(&["nope_col"]).compile().unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope_col"),
        "got: {err:?}"
    );
}

#[test]
fn only_empty_list_errors_at_compile() {
    // `.only(&[])` is asking for "no columns" — same shape as
    // `.values_dict(&[])`, same error.
    let err = Post::objects().only(&[]).compile().unwrap_err();
    assert!(
        matches!(err, QueryError::EmptyValuesProjection),
        "got: {err:?}"
    );
}

#[test]
fn defer_and_only_compose_with_order_limit() {
    let q = Post::objects()
        .order_by(&[("id", false)])
        .limit(5)
        .only(&["id", "title"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(stmt.sql.contains("ORDER BY \"id\""), "{}", stmt.sql);
    assert!(stmt.sql.contains("LIMIT 5"), "{}", stmt.sql);
}
