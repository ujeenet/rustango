//! Cookbook Chapter 7b — DRF-shape serializers.
//!
//! `#[derive(Serializer)]` + `#[serializer(model = Author)]` produces
//! a typed JSON façade over the model — read-only / write-only /
//! source-renamed / skip per-field, plus a `validate()` hook for
//! cross-field rules.
//!
//! No DB needed — pure mapping + serde. Run:
//! `cargo test --test cookbook_chapter07b_serializer`

use cookbook_blog::apps::blog::models::Author;
use rustango::serializer::ModelSerializer;
use rustango::sql::Auto;
use rustango::Serializer;

/// §7.99b — basic Serializer mirrors every Author field.
#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Author)]
pub struct AuthorSerializer {
    pub id: Auto<i64>,
    pub name: String,
    pub email: String,
    pub bio: Option<String>,
    pub joined_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// §7.99b — read_only marks fields excluded from `writable_fields()`,
/// write_only marks fields excluded from JSON output, source = "x"
/// renames the JSON key to a different model field, skip drops the
/// field from both directions.
#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Author)]
pub struct AuthorPublicSerializer {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    pub name: String,
    /// Renamed in the JSON payload. The DB column / model field is
    /// still `email`; clients see `contact_email`.
    #[serializer(source = "email")]
    pub contact_email: String,
    pub bio: Option<String>,
    /// Server-set; not exposed to writers; read-only in JSON.
    #[serializer(read_only)]
    pub joined_at: Auto<chrono::DateTime<chrono::Utc>>,
    /// Excluded from JSON output. Useful for write-only secrets.
    #[serializer(write_only)]
    pub admin_token: String,
    /// Skipped in both directions; populate manually after `from_model`.
    #[serializer(skip)]
    pub note: String,
}

fn fixture_author() -> Author {
    Author {
        id: Auto::Set(42),
        name: "ada".into(),
        email: "ada@example.com".into(),
        bio: Some("first programmer".into()),
        joined_at: Auto::Set(chrono::Utc.with_ymd_and_hms(2026, 5, 4, 0, 0, 0).unwrap()),
    }
}

use chrono::TimeZone as _;

// §7.99b — from_model copies every field; to_value emits the JSON shape.
#[test]
fn serializer_from_model_then_to_value_round_trip() {
    let a = fixture_author();
    let s = AuthorSerializer::from_model(&a);
    assert!(matches!(s.id, Auto::Set(42)));
    assert_eq!(s.name, "ada");
    assert_eq!(s.email, "ada@example.com");

    let json = s.to_value();
    assert_eq!(json["name"], "ada");
    assert_eq!(json["email"], "ada@example.com");
    assert_eq!(json["bio"], "first programmer");
}

// §7.99b — read_only excludes the field from writable_fields().
#[test]
fn read_only_field_omitted_from_writable_fields() {
    let writable = AuthorPublicSerializer::writable_fields();
    assert!(!writable.contains(&"id"), "read_only id must be excluded; got {writable:?}");
    assert!(!writable.contains(&"joined_at"), "read_only joined_at must be excluded");
    // Renamed `contact_email` IS writable.
    assert!(writable.contains(&"contact_email"));
    assert!(writable.contains(&"name"));
    assert!(writable.contains(&"bio"));
}

// §7.99b — write_only field is excluded from JSON output.
#[test]
fn write_only_field_excluded_from_json_output() {
    let mut s = AuthorPublicSerializer::from_model(&fixture_author());
    s.admin_token = "leaked".into();
    let json = s.to_value();
    assert!(json.get("admin_token").is_none(), "write_only must not appear in JSON");
    // But it IS writable on input.
    assert!(AuthorPublicSerializer::writable_fields().contains(&"admin_token"));
}

// §7.99b — source = "x" renames the JSON key to a different model field.
#[test]
fn source_attribute_renames_json_key() {
    let s = AuthorPublicSerializer::from_model(&fixture_author());
    let json = s.to_value();
    // JSON has the renamed key `contact_email`, model field is still email.
    assert_eq!(json["contact_email"], "ada@example.com");
    assert!(json.get("email").is_none(), "model field name must not leak into JSON");
}

// §7.99b — skip drops the field from both serialize AND from_model copy.
#[test]
fn skip_field_uses_default_and_appears_in_json_unchanged() {
    let s = AuthorPublicSerializer::from_model(&fixture_author());
    // skip uses Default::default() — String::default() == "".
    assert_eq!(s.note, "");
    let json = s.to_value();
    // skip leaves the field in JSON output (with the Default value)
    // but excludes it from writable_fields.
    assert_eq!(json["note"], "");
    assert!(!AuthorPublicSerializer::writable_fields().contains(&"note"));
}

// §7.99b — many_to_value batches a Vec<Model> into a JSON array.
#[test]
fn many_to_value_batches_into_json_array() {
    let authors = vec![fixture_author(), Author {
        name: "bob".into(),
        email: "bob@example.com".into(),
        ..fixture_author()
    }];
    let arr = AuthorSerializer::many_to_value(&authors);
    let arr = arr.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "ada");
    assert_eq!(arr[1]["name"], "bob");
}
