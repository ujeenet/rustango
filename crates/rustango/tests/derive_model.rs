//! End-to-end check that `#[derive(Model)]` emits the expected schema and
//! that the model is discoverable via the inventory registry.

use rustango::core::{inventory, FieldType, Model as _, ModelEntry};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "auth_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(column = "user_name")]
    name: String,
    email: Option<String>,
    is_active: bool,
}

#[derive(Model)]
#[allow(dead_code)]
pub struct BlogPost {
    #[rustango(primary_key)]
    id: i64,
    title: String,
}

#[test]
fn schema_reflects_attributes() {
    let s = User::SCHEMA;
    assert_eq!(s.name, "User");
    assert_eq!(s.table, "auth_user");
    assert_eq!(s.fields.len(), 4);

    let id = s.field("id").unwrap();
    assert_eq!(id.column, "id");
    assert_eq!(id.ty, FieldType::I64);
    assert!(id.primary_key);
    assert!(!id.nullable);

    let name = s.field("name").unwrap();
    assert_eq!(name.column, "user_name");
    assert_eq!(name.ty, FieldType::String);
    assert!(!name.nullable);

    let email = s.field("email").unwrap();
    assert_eq!(email.ty, FieldType::String);
    assert!(email.nullable);

    assert_eq!(s.primary_key().map(|f| f.name), Some("id"));
}

#[test]
fn default_table_is_snake_case() {
    assert_eq!(BlogPost::SCHEMA.table, "blog_post");
}

#[test]
fn models_register_with_inventory() {
    let registered: Vec<&'static str> = inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema.name)
        .collect();
    assert!(
        registered.contains(&"User"),
        "User missing from registry: {registered:?}"
    );
    assert!(
        registered.contains(&"BlogPost"),
        "BlogPost missing from registry: {registered:?}",
    );
}
