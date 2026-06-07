//! Smoke test for the proc-macro re-export pattern of issue
//! [#143](https://github.com/ujeenet/rustango/issues/143).
//!
//! Consumers should be able to `use rustango_orm_macros::Model;` and
//! have `#[derive(Model)]` resolve through this crate's re-export.
//! If the resolver couldn't follow `pub use` paths to the underlying
//! proc-macro definition, this file wouldn't compile.
//!
//! Gated on `feature = "postgres"` for the same reason
//! `rustango-renamed-smoke` is — the macro emits
//! `#[cfg(feature = "postgres")] impl LoadRelated for #Struct`
//! against the CONSUMER's feature flags, and the consumer here
//! is the integration-test binary, which inherits the rustango
//! workspace's all-features config in CI but might not in a
//! sqlite-only litmus.

#![cfg(feature = "postgres")]
#![allow(dead_code)]

use rustango::sql::Auto;
use rustango_orm_macros::{Form, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "rom_post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Form, Debug)]
pub struct PostForm {
    #[form(min_length = 1, max_length = 200)]
    pub title: String,
}

#[test]
fn model_derive_reexports_through_orm_macros_crate() {
    use rustango::core::Model as _;
    assert_eq!(Post::SCHEMA.table, "rom_post");
    assert!(Post::SCHEMA.primary_key().is_some());
}

#[test]
fn form_derive_reexports_through_orm_macros_crate() {
    use rustango::forms::Form;
    let mut payload = std::collections::HashMap::new();
    payload.insert("title".to_string(), "hello".to_string());
    let f = PostForm::parse(&payload).expect("valid");
    assert_eq!(f.title, "hello");
}
