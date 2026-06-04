//! Django parity — `Meta.proxy = True` flag. rustango spells the
//! attribute as `#[rustango(proxy)]` (bare → true) or
//! `#[rustango(proxy = true | false)]`. Stored on
//! `ModelSchema::proxy`.
//!
//! Declarative-only today: migration / admin / DRF surfaces still
//! treat every model as table-owning. The metadata is the
//! foundation for skipping `CreateTable` emission on proxies (parent
//! owns the table) and for routing per-instance method resolution to
//! the proxy class. rustango's `crate::inheritance` extension-trait
//! pattern is the working idiom for the same shape today.

use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mp_post", proxy)]
#[allow(dead_code)]
pub struct ArchivedPost {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mp_post_explicit_true", proxy = true)]
#[allow(dead_code)]
pub struct ExplicitTrue {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mp_post_explicit_false", proxy = false)]
#[allow(dead_code)]
pub struct ExplicitFalse {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mp_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn bare_proxy_flag_means_true() {
    let schema = <ArchivedPost as rustango::core::Model>::SCHEMA;
    assert!(schema.proxy);
}

#[test]
fn explicit_true_round_trips() {
    let schema = <ExplicitTrue as rustango::core::Model>::SCHEMA;
    assert!(schema.proxy);
}

#[test]
fn explicit_false_round_trips() {
    let schema = <ExplicitFalse as rustango::core::Model>::SCHEMA;
    assert!(!schema.proxy);
}

#[test]
fn plain_model_defaults_to_non_proxy() {
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert!(!plain.proxy);
}
