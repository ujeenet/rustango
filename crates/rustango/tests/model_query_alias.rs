//! Compile + behaviour test for the macro-emitted
//! `Model::query()` alias of `Model::objects()`. Eloquent muscle-
//! memory alias; both point at the same constructor.

use rustango::core::SqlValue;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "mq_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
}

#[test]
fn query_and_objects_compile_equivalently() {
    let qs_a = Post::query().filter("title", "hello");
    let qs_b = Post::objects().filter("title", "hello");

    let sql_a = Postgres
        .compile_select(&qs_a.compile().unwrap())
        .unwrap()
        .sql;
    let sql_b = Postgres
        .compile_select(&qs_b.compile().unwrap())
        .unwrap()
        .sql;

    assert_eq!(
        sql_a, sql_b,
        "query() and objects() must produce the same SQL"
    );
    assert!(sql_a.contains(r#""title" = $1"#), "got: {sql_a}");
}

#[test]
fn query_returns_chainable_queryset() {
    let qs = Post::query().filter("title", "x").limit(5);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#""title" = $1"#));
    assert!(stmt.sql.contains("LIMIT 5"));
    assert_eq!(stmt.params, vec![SqlValue::String("x".into())]);
}
