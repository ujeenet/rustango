//! Django parity — `Meta.default_related_name` lets a model override
//! the convention reverse-relation managers use when an FK / M2M
//! field doesn't pass `related_name="..."` itself.
//!
//! rustango spells the attribute as
//! `#[rustango(default_related_name = "...")]` on the model container.
//! The value is stored on `ModelSchema::default_related_name` and the
//! macro validates snake_case ASCII identifier shape at derive time so
//! the string is safe for any future code that turns it back into an
//! ident (reverse-manager codegen, DRF schema emit, admin templates).
//!
//! Today rustango doesn't auto-emit reverse managers; this PR lays the
//! declarative foundation.

use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "drn_post", default_related_name = "posts")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    pub author_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "drn_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "drn_archive_item", default_related_name = "archive_items_v2")]
#[allow(dead_code)]
pub struct ArchiveItem {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn schema_carries_snake_case_related_name() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    assert_eq!(schema.default_related_name, Some("posts"));
}

#[test]
fn underscore_and_digit_chars_allowed() {
    let schema = <ArchiveItem as rustango::core::Model>::SCHEMA;
    assert_eq!(schema.default_related_name, Some("archive_items_v2"));
}

#[test]
fn plain_model_has_none() {
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert!(plain.default_related_name.is_none());
}
