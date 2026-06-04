//! Django parity — `Meta.order_with_respect_to = "parent_fk"` names
//! the FK field this model's instances are ordered relative to.
//! Django auto-generates a `_order` integer column + admin
//! reordering UI when set.
//!
//! rustango spells the attribute as
//! `#[rustango(order_with_respect_to = "...")]` on the model
//! container. Stored on `ModelSchema::order_with_respect_to`.
//! Declarative-only today; future codegen will auto-emit the
//! `_order` column and reorder helpers.

use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "owrt_section_item", order_with_respect_to = "section_id")]
#[allow(dead_code)]
pub struct SectionItem {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "owrt_section", on = "id")]
    pub section_id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "owrt_section")]
#[allow(dead_code)]
pub struct Section {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[test]
fn schema_carries_order_with_respect_to_field_name() {
    let schema = <SectionItem as rustango::core::Model>::SCHEMA;
    assert_eq!(schema.order_with_respect_to, Some("section_id"));
}

#[test]
fn plain_model_has_none() {
    let plain = <Section as rustango::core::Model>::SCHEMA;
    assert!(plain.order_with_respect_to.is_none());
}
