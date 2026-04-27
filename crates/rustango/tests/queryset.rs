//! Covers FK/O2O attributes, the inherent `objects()` shortcut, and
//! `QuerySet::compile()` validation against the schema.

use rustango::core::{FieldType, Model as _, Op, QueryError, Relation, SqlValue};
use rustango::Model;

#[derive(Model)]
#[allow(dead_code)]
struct User {
    #[rustango(primary_key)]
    id: i64,
    name: String,
    is_active: bool,
}

#[derive(Model)]
#[allow(dead_code)]
struct BlogPost {
    #[rustango(primary_key)]
    id: i64,
    title: String,
    #[rustango(fk = "user", on = "id")]
    author_id: i64,
}

#[derive(Model)]
#[allow(dead_code)]
struct UserProfile {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(o2o = "user")]
    user_id: i64,
    bio: String,
}

#[test]
fn fk_relation_lands_on_field_schema() {
    let f = BlogPost::SCHEMA.field("author_id").unwrap();
    match f.relation {
        Some(Relation::Fk { to, on }) => {
            assert_eq!(to, "user");
            assert_eq!(on, "id");
        }
        other => panic!("expected Fk, got {other:?}"),
    }
}

#[test]
fn o2o_defaults_on_to_id() {
    let f = UserProfile::SCHEMA.field("user_id").unwrap();
    match f.relation {
        Some(Relation::O2O { to, on }) => {
            assert_eq!(to, "user");
            assert_eq!(on, "id");
        }
        other => panic!("expected O2O, got {other:?}"),
    }
}

#[test]
fn objects_compiles_with_resolved_columns() {
    let q = User::objects()
        .eq("name", "alice")
        .filter("is_active", Op::Eq, true)
        .compile()
        .unwrap();

    assert_eq!(q.model.name, "User");
    assert_eq!(q.filters.len(), 2);
    assert_eq!(q.filters[0].column, "name");
    assert_eq!(q.filters[0].op, Op::Eq);
    assert_eq!(q.filters[0].value, SqlValue::String("alice".into()));
    assert_eq!(q.filters[1].column, "is_active");
    assert_eq!(q.filters[1].value, SqlValue::Bool(true));
}

#[test]
fn unknown_field_is_rejected_at_compile() {
    let err = User::objects().eq("nope", 1_i64).compile().unwrap_err();
    assert_eq!(
        err,
        QueryError::UnknownField {
            model: "User",
            field: "nope".into()
        }
    );
}

#[test]
fn type_mismatch_is_rejected_at_compile() {
    let err = User::objects().eq("name", 42_i64).compile().unwrap_err();
    assert_eq!(
        err,
        QueryError::TypeMismatch {
            model: "User",
            field: "name".into(),
            expected: FieldType::String,
            actual: FieldType::I64,
        }
    );
}

#[test]
fn null_value_skips_type_check() {
    let q = User::objects()
        .filter("name", Op::Eq, Option::<String>::None)
        .compile()
        .unwrap();
    assert_eq!(q.filters[0].value, SqlValue::Null);
}
