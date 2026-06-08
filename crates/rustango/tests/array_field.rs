//! Unit coverage for `Array<T>` PostgreSQL array columns — Django
//! `ArrayField` (#341). No database required: asserts the derived
//! schema's `FieldType::Array` mapping and the per-dialect column-type
//! emission (`text[]` / `integer[]` / `bigint[]` on PG; degraded `TEXT`
//! on MySQL / SQLite, where arrays are unsupported by language).

use rustango::core::{ArrayElem, FieldType, Model as _};
use rustango::sql::{Array, Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "arr_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    tags: Array<String>,
    scores: Array<i32>,
    big_scores: Array<i64>,
}

fn field(name: &str) -> &'static rustango::core::FieldSchema {
    Post::SCHEMA
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

#[test]
fn derive_maps_array_fields_to_array_field_type() {
    assert_eq!(field("tags").ty, FieldType::Array(ArrayElem::Text));
    assert_eq!(field("scores").ty, FieldType::Array(ArrayElem::Int));
    assert_eq!(field("big_scores").ty, FieldType::Array(ArrayElem::BigInt));
    // Sanity: a plain String field is unaffected.
    assert_eq!(field("title").ty, FieldType::String);
}

#[test]
fn postgres_column_types_are_native_arrays() {
    assert_eq!(Postgres.column_type(field("tags").ty, None), "text[]");
    assert_eq!(Postgres.column_type(field("scores").ty, None), "integer[]");
    assert_eq!(
        Postgres.column_type(field("big_scores").ty, None),
        "bigint[]"
    );
}

#[test]
fn postgres_cast_types_are_native_arrays() {
    assert_eq!(
        Postgres.cast_type(FieldType::Array(ArrayElem::Text)),
        Some("text[]")
    );
    assert_eq!(
        Postgres.cast_type(FieldType::Array(ArrayElem::Int)),
        Some("integer[]")
    );
    assert_eq!(
        Postgres.cast_type(FieldType::Array(ArrayElem::BigInt)),
        Some("bigint[]")
    );
}

#[test]
fn non_pg_dialects_degrade_to_text() {
    // Arrays are PG-only by language semantics — MySQL / SQLite have no
    // native array column type, so the DDL writer emits TEXT (the bind /
    // decode paths error on those backends).
    for ty in [
        FieldType::Array(ArrayElem::Text),
        FieldType::Array(ArrayElem::Int),
        FieldType::Array(ArrayElem::BigInt),
    ] {
        assert_eq!(MySql.column_type(ty, None), "TEXT");
        assert_eq!(Sqlite.column_type(ty, None), "TEXT");
        // No native CAST target on MySQL.
        assert_eq!(MySql.cast_type(ty), None);
    }
}

#[test]
fn array_element_type_tokens() {
    assert_eq!(ArrayElem::Text.pg_element_type(), "text");
    assert_eq!(ArrayElem::Int.pg_element_type(), "integer");
    assert_eq!(ArrayElem::BigInt.pg_element_type(), "bigint");
}

#[test]
fn array_into_sqlvalue_is_single_param_array() {
    use rustango::core::SqlValue;
    let v: SqlValue = Array(vec!["rust".to_owned(), "orm".to_owned()]).into();
    match v {
        SqlValue::Array(items) => assert_eq!(items.len(), 2),
        other => panic!("expected SqlValue::Array, got {other:?}"),
    }
}
