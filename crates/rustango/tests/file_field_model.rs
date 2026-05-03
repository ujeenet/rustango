//! `FileField` integration: derives `Model` with one and verifies
//! the macro classifies it as `FieldType::String` (so migrations
//! emit a TEXT column) without changing any other model behavior.

use rustango::core::{FieldType, Model as _};
use rustango::file_field::FileField;
use rustango::Model;

#[derive(Model, Clone)]
#[rustango(table = "users_with_avatar")]
#[allow(dead_code)]
pub struct UserWithAvatar {
    #[rustango(primary_key)]
    pub id: i64,
    pub name: String,
    pub avatar: Option<FileField>,
    pub thumbnail: FileField,
}

#[test]
fn file_field_columns_classified_as_string_in_schema() {
    let s = UserWithAvatar::SCHEMA;

    let avatar = s.field("avatar").expect("avatar field on schema");
    assert!(avatar.nullable, "Option<FileField> -> nullable column");
    assert_eq!(
        avatar.ty,
        FieldType::String,
        "FileField stored as String / TEXT"
    );

    let thumb = s.field("thumbnail").expect("thumbnail field on schema");
    assert!(!thumb.nullable, "bare FileField -> NOT NULL");
    assert_eq!(thumb.ty, FieldType::String);
}

#[test]
fn instance_round_trips_through_filefield_construction() {
    // This is mostly a compile-time check: building an instance with
    // FileField fields must work without ceremony.
    let u = UserWithAvatar {
        id: 1,
        name: "Alice".into(),
        avatar: Some(FileField::new("avatars/alice.png")),
        thumbnail: FileField::new("thumbnails/alice.png"),
    };
    assert_eq!(u.thumbnail.key(), "thumbnails/alice.png");
    assert_eq!(
        u.avatar.as_ref().map(|f| f.key()),
        Some("avatars/alice.png")
    );
}

#[test]
fn other_existing_field_kinds_still_work_alongside_filefield() {
    // Catches any regression where extending the kind table broke
    // detection of the existing types.
    let s = UserWithAvatar::SCHEMA;
    let id = s.field("id").unwrap();
    assert_eq!(id.ty, FieldType::I64);
    assert!(!id.nullable);
    assert!(id.primary_key);
    let name = s.field("name").unwrap();
    assert_eq!(name.ty, FieldType::String);
    assert!(!name.nullable);
}
