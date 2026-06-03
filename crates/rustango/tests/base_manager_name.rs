//! Django parity — `Meta.base_manager_name` names the Manager
//! subclass that `<instance>.<relation>_set` uses when resolving
//! reverse-relation managers (distinct from `default_manager_name`,
//! which is what `Model.objects` returns at the class level).
//!
//! rustango spells the attribute as
//! `#[rustango(base_manager_name = "...")]` on the model container.
//! Stored on `ModelSchema::base_manager_name`. Declarative-only today;
//! future reverse-manager codegen + DRF schema emit read the
//! metadata directly off the schema.

use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "bmn_post", base_manager_name = "PostManagerExt")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "bmn_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn schema_carries_base_manager_name() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    assert_eq!(schema.base_manager_name, Some("PostManagerExt"));
}

#[test]
fn plain_model_has_none() {
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert!(plain.base_manager_name.is_none());
}
