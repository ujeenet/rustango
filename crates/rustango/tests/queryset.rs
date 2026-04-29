//! Covers FK/O2O attributes, the inherent `objects()` shortcut, and
//! `QuerySet::compile()` validation against the schema.

use rustango::core::{FieldType, Filter, Model as _, Op, QueryError, Relation, SqlValue};
use rustango::Model;

#[derive(Model)]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    name: String,
    is_active: bool,
}

#[derive(Model)]
#[allow(dead_code)]
pub struct BlogPost {
    #[rustango(primary_key)]
    id: i64,
    title: String,
    #[rustango(fk = "user", on = "id")]
    author_id: i64,
}

#[derive(Model)]
#[allow(dead_code)]
pub struct UserProfile {
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
    let filters: Vec<&Filter> = q.where_clause.as_flat_and().unwrap();
    assert_eq!(filters.len(), 2);
    assert_eq!(filters[0].column, "name");
    assert_eq!(filters[0].op, Op::Eq);
    assert_eq!(filters[0].value, SqlValue::String("alice".into()));
    assert_eq!(filters[1].column, "is_active");
    assert_eq!(filters[1].value, SqlValue::Bool(true));
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
    assert_eq!(q.where_clause.as_flat_and().unwrap()[0].value, SqlValue::Null);
}

// ---------------- compile_delete ----------------

#[test]
fn compile_delete_resolves_filter_columns() {
    let query = User::objects()
        .eq("name", "alice")
        .compile_delete()
        .unwrap();
    assert_eq!(query.model.name, "User");
    let filters = query.where_clause.as_flat_and().unwrap();
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].column, "name");
    assert_eq!(filters[0].value, SqlValue::String("alice".into()));
}

#[test]
fn compile_delete_with_no_filters_is_valid() {
    let query = User::objects().compile_delete().unwrap();
    assert!(query.where_clause.is_empty());
}

#[test]
fn compile_delete_rejects_unknown_field() {
    let err = User::objects()
        .eq("nope", 1_i64)
        .compile_delete()
        .unwrap_err();
    assert_eq!(
        err,
        QueryError::UnknownField {
            model: "User",
            field: "nope".into()
        }
    );
}

// ---------------- UpdateBuilder ----------------

#[test]
fn update_builder_accumulates_set_assignments() {
    let query = User::objects()
        .eq("name", "alice")
        .update()
        .set("is_active", false)
        .set("name", "ALICE")
        .compile()
        .unwrap();

    assert_eq!(query.set.len(), 2);
    assert_eq!(query.set[0].column, "is_active");
    assert_eq!(query.set[0].value, SqlValue::Bool(false));
    assert_eq!(query.set[1].column, "name");
    assert_eq!(query.set[1].value, SqlValue::String("ALICE".into()));

    let filters = query.where_clause.as_flat_and().unwrap();
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].column, "name");
}

#[test]
fn update_builder_rejects_unknown_set_field() {
    let err = User::objects()
        .update()
        .set("nope", 1_i64)
        .compile()
        .unwrap_err();
    assert_eq!(
        err,
        QueryError::UnknownField {
            model: "User",
            field: "nope".into()
        }
    );
}

#[test]
fn update_builder_rejects_set_type_mismatch() {
    let err = User::objects()
        .update()
        .set("name", 42_i64)
        .compile()
        .unwrap_err();
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
fn update_builder_propagates_filter_errors() {
    let err = User::objects()
        .eq("nope", 1_i64)
        .update()
        .set("is_active", false)
        .compile()
        .unwrap_err();
    assert!(matches!(err, QueryError::UnknownField { .. }));
}

#[test]
fn update_builder_with_no_filters_compiles() {
    let query = User::objects()
        .update()
        .set("is_active", false)
        .compile()
        .unwrap();
    assert!(query.where_clause.is_empty());
    assert_eq!(query.set.len(), 1);
}

// ---------------- limit / offset ----------------

#[test]
fn limit_lands_on_select_query() {
    let q = User::objects().limit(10).compile().unwrap();
    assert_eq!(q.limit, Some(10));
    assert_eq!(q.offset, None);
}

#[test]
fn offset_lands_on_select_query() {
    let q = User::objects().offset(20).compile().unwrap();
    assert_eq!(q.offset, Some(20));
    assert_eq!(q.limit, None);
}

#[test]
fn limit_and_offset_chain() {
    let q = User::objects().limit(5).offset(10).compile().unwrap();
    assert_eq!(q.limit, Some(5));
    assert_eq!(q.offset, Some(10));
}

#[test]
fn last_limit_wins() {
    let q = User::objects().limit(5).limit(99).compile().unwrap();
    assert_eq!(q.limit, Some(99));
}
