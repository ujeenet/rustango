#![cfg(feature = "casts")]
//! Unit coverage for attribute casts (#819): a `Cast<C>` field maps to a
//! plain `TEXT` column (`FieldType::String`) — the `CastValue` impl does
//! the logical↔stored transform at bind/decode time, so no special DDL.

use rustango::casts::{Cast, EncryptedString};
use rustango::core::{FieldType, Model as _};
use rustango::sql::{Auto, Dialect, Postgres, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "cast_patient")]
#[allow(dead_code)]
pub struct Patient {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    pub ssn: Cast<EncryptedString>,
}

fn field(name: &str) -> &'static rustango::core::FieldSchema {
    Patient::SCHEMA
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

#[test]
fn cast_field_maps_to_text_column() {
    assert_eq!(field("ssn").ty, FieldType::String);
    // Stored as TEXT on PG; TEXT affinity on SQLite — an ordinary text
    // column, identical to a plain String field.
    assert_eq!(Postgres.column_type(field("ssn").ty, None), "TEXT");
    assert_eq!(Sqlite.column_type(field("ssn").ty, None), "TEXT");
}

#[test]
fn cast_wraps_and_derefs_logical_value() {
    let c: Cast<EncryptedString> = Cast::new("hi".to_owned());
    assert_eq!(&*c, "hi");
    assert_eq!(c.into_inner(), "hi");
}
