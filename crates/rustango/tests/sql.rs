//! End-to-end check of the `QuerySet` → `SelectQuery` → Postgres SQL pipeline.

use rustango::core::{InsertQuery, Model as _, Op, SqlValue};
use rustango::sql::{Dialect, Postgres, SqlError};
use rustango::Model;

#[derive(Model)]
#[allow(dead_code)]
struct User {
    #[rustango(primary_key)]
    id: i64,
    name: String,
    is_active: bool,
}

fn pg() -> Postgres {
    Postgres
}

#[test]
fn select_with_no_filters_lists_scalar_columns() {
    let stmt = pg()
        .compile_select(&User::objects().compile().unwrap())
        .unwrap();
    assert_eq!(stmt.sql, r#"SELECT "id", "name", "is_active" FROM "user""#);
    assert!(stmt.params.is_empty());
}

#[test]
fn equality_filter_emits_dollar_placeholder() {
    let stmt = pg()
        .compile_select(&User::objects().eq("name", "alice").compile().unwrap())
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" = $1"#
    );
    assert_eq!(stmt.params, vec![SqlValue::String("alice".into())]);
}

#[test]
fn multiple_filters_join_with_and_and_increment_placeholders() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .eq("name", "alice")
                .filter("is_active", Op::Eq, true)
                .filter("id", Op::Gt, 10_i64)
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" = $1 AND "is_active" = $2 AND "id" > $3"#
    );
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::String("alice".into()),
            SqlValue::Bool(true),
            SqlValue::I64(10),
        ]
    );
}

#[test]
fn is_null_does_not_consume_placeholder() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .filter("name", Op::IsNull, true)
                .filter("id", Op::Eq, 1_i64)
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" IS NULL AND "id" = $1"#
    );
    assert_eq!(stmt.params, vec![SqlValue::I64(1)]);
}

#[test]
fn is_not_null_emitted_for_false() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .filter("name", Op::IsNull, false)
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" IS NOT NULL"#
    );
}

#[test]
fn in_list_expands_to_one_placeholder_per_element() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .filter(
                    "id",
                    Op::In,
                    SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)]),
                )
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "id" IN ($1, $2, $3)"#
    );
    assert_eq!(
        stmt.params,
        vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)],
    );
}

#[test]
fn empty_in_list_is_rejected() {
    let err = pg()
        .compile_select(
            &User::objects()
                .filter("id", Op::In, SqlValue::List(vec![]))
                .compile()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(err, SqlError::EmptyInList));
}

#[test]
fn in_with_non_list_is_rejected() {
    let err = pg()
        .compile_select(
            &User::objects()
                .filter("id", Op::In, 1_i64)
                .compile()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(err, SqlError::InRequiresList));
}

#[test]
fn is_null_with_non_bool_is_rejected() {
    let err = pg()
        .compile_select(
            &User::objects()
                .filter("name", Op::IsNull, "alice")
                .compile()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(err, SqlError::IsNullRequiresBool));
}

#[test]
fn insert_emits_columns_and_placeholders() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["id", "name", "is_active"],
        values: vec![
            SqlValue::I64(7),
            SqlValue::String("alice".into()),
            SqlValue::Bool(true),
        ],
    };
    let stmt = pg().compile_insert(&query).unwrap();
    assert_eq!(
        stmt.sql,
        r#"INSERT INTO "user" ("id", "name", "is_active") VALUES ($1, $2, $3)"#,
    );
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::I64(7),
            SqlValue::String("alice".into()),
            SqlValue::Bool(true),
        ],
    );
}

#[test]
fn insert_with_no_columns_is_rejected() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec![],
        values: vec![],
    };
    let err = pg().compile_insert(&query).unwrap_err();
    assert!(matches!(err, SqlError::EmptyInsert));
}

#[test]
fn insert_with_mismatched_lengths_is_rejected() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["id"],
        values: vec![SqlValue::I64(1), SqlValue::I64(2)],
    };
    let err = pg().compile_insert(&query).unwrap_err();
    assert!(matches!(
        err,
        SqlError::InsertShapeMismatch {
            columns: 1,
            values: 2
        }
    ));
}
