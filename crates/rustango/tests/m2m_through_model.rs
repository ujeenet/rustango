//! Django-parity #324 — `ManyToManyField(through=<custom model>)`.
//!
//! When `auto_create = false`, the migration writer skips emitting a
//! `CREATE TABLE` for the junction so the operator can declare their
//! own through model (with extra columns) via a separate
//! `#[derive(Model)]`.

#![cfg(feature = "sqlite")]

use rustango::core::Model as _;
use rustango::migrate::SchemaSnapshot;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "m2mthrough_person")]
#[allow(dead_code)]
pub struct M2mthroughPerson {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    name: String,
}

// Through model with extra columns — operator owns the table.
#[derive(Model, Debug, Clone)]
#[rustango(table = "m2mthrough_membership")]
#[allow(dead_code)]
pub struct M2mthroughMembership {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    person_id: i64,
    group_id: i64,
    date_joined: chrono::NaiveDate,
}

// Group declares an M2M with auto_create=false — the migration writer
// should NOT emit a duplicate junction table.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2mthrough_group",
    m2m(
        name = "members",
        to = "m2mthrough_person",
        through = "m2mthrough_membership",
        src = "group_id",
        dst = "person_id",
        auto_create = false,
    )
)]
#[allow(dead_code)]
pub struct M2mthroughGroup {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    name: String,
}

// Sanity baseline — a plain M2M (auto_create defaults to true) DOES
// produce a junction-table snapshot entry. Same shape but a different
// pair of source/target tables.
#[derive(Model, Debug, Clone)]
#[rustango(table = "m2mthrough_tag")]
#[allow(dead_code)]
pub struct M2mthroughTag {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    label: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "m2mthrough_post",
    m2m(
        name = "tags",
        to = "m2mthrough_tag",
        through = "m2mthrough_post_tags",
        src = "post_id",
        dst = "tag_id",
    )
)]
#[allow(dead_code)]
pub struct M2mthroughPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

#[test]
fn auto_create_default_is_true() {
    let m2m = M2mthroughPost::SCHEMA.m2m.first().expect("one relation");
    assert!(m2m.auto_create, "default should be true");
}

#[test]
fn auto_create_false_records_on_schema() {
    let m2m = M2mthroughGroup::SCHEMA.m2m.first().expect("one relation");
    assert!(!m2m.auto_create);
}

#[test]
fn snapshot_skips_auto_create_false_junction() {
    let snap = SchemaSnapshot::from_models(&[
        M2mthroughGroup::SCHEMA,
        M2mthroughPerson::SCHEMA,
        M2mthroughMembership::SCHEMA,
        M2mthroughPost::SCHEMA,
        M2mthroughTag::SCHEMA,
    ]);
    // The operator-declared through model `m2mthrough_membership` IS
    // present as a regular table (it's a `#[derive(Model)]`).
    assert!(
        snap.table("m2mthrough_membership").is_some(),
        "operator-owned through model must appear in the table snapshot",
    );
    // ...but the snapshot's M2M junction list must NOT also include
    // it (that would emit a duplicate CREATE TABLE).
    let junction_names: Vec<&str> = snap.m2m_tables.iter().map(|t| t.through.as_str()).collect();
    assert!(
        !junction_names.contains(&"m2mthrough_membership"),
        "auto_create=false junction must not appear in m2m_tables: {junction_names:?}",
    );
    // The plain M2M's junction (auto_create defaulted to true) IS
    // present — sanity check on the filter.
    assert!(
        junction_names.contains(&"m2mthrough_post_tags"),
        "auto_create=true junction missing: {junction_names:?}",
    );
}
