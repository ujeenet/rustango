//! Unit coverage for `HStore` PostgreSQL hstore columns — Django
//! `HStoreField` (#342). No database required: asserts the derived
//! schema's `FieldType::HStore` mapping, the per-dialect column-type
//! emission (`hstore` on PG; degraded `TEXT` on MySQL / SQLite), and the
//! `Into<SqlValue>` lowering.

use rustango::core::{FieldType, Model as _, SqlValue};
use rustango::sql::{Dialect, HStore, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "hs_product")]
#[allow(dead_code)]
pub struct Product {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    name: String,
    attrs: HStore,
}

fn field(name: &str) -> &'static rustango::core::FieldSchema {
    Product::SCHEMA
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

#[test]
fn derive_maps_hstore_field_to_hstore_field_type() {
    assert_eq!(field("attrs").ty, FieldType::HStore);
    assert_eq!(field("name").ty, FieldType::String);
}

#[test]
fn postgres_column_type_is_hstore() {
    assert_eq!(Postgres.column_type(field("attrs").ty, None), "hstore");
    assert_eq!(Postgres.cast_type(FieldType::HStore), Some("hstore"));
}

#[test]
fn non_pg_dialects_degrade_to_text() {
    assert_eq!(MySql.column_type(FieldType::HStore, None), "TEXT");
    assert_eq!(Sqlite.column_type(FieldType::HStore, None), "TEXT");
    assert_eq!(MySql.cast_type(FieldType::HStore), None);
}

#[test]
fn hstore_into_sqlvalue_is_pair_list() {
    let v: SqlValue = HStore::from_iter([("color", "red")]).into();
    match v {
        SqlValue::HStore(pairs) => {
            assert_eq!(pairs, vec![("color".to_owned(), Some("red".to_owned()))]);
        }
        other => panic!("expected SqlValue::HStore, got {other:?}"),
    }
}

#[test]
fn hstore_column_is_bound_as_a_single_param() {
    // hstore binds natively (no text-literal cast gymnastics) — the
    // INSERT just emits a plain placeholder for the column.
    use rustango::core::InsertQuery;
    let q = InsertQuery {
        model: Product::SCHEMA,
        columns: vec!["attrs"],
        values: vec![SqlValue::HStore(vec![(
            "k".to_owned(),
            Some("v".to_owned()),
        )])],
        returning: vec![],
        on_conflict: None,
    };
    let stmt = Postgres.compile_insert(&q).unwrap();
    assert!(stmt.sql.contains("VALUES ($1)"), "sql: {}", stmt.sql);
    assert!(matches!(&stmt.params[0], SqlValue::HStore(_)));
}
