//! `#[rustango(view)]` marker — closes #293 / T2.10.
//!
//! Pins:
//!   1. The flag parses and lands on `ModelSchema::is_view`.
//!   2. View-backed models are excluded from `SchemaSnapshot::from_models`
//!      (the snapshot is what `makemigrations` diffs against, so the
//!      migration runner never emits CREATE/DROP TABLE for the view).
//!   3. Default `is_view = false` when the attribute isn't present.

use rustango::core::Model as _;
use rustango::migrate::snapshot::SchemaSnapshot;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vmk_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 100)]
    title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "vmk_big_post", view)]
#[allow(dead_code)]
pub struct BigPost {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 100)]
    title: String,
    views: i64,
}

#[test]
fn view_flag_lands_on_schema() {
    assert!(BigPost::SCHEMA.is_view, "view flag must set is_view = true");
    assert!(
        !Post::SCHEMA.is_view,
        "no flag must default to is_view = false"
    );
}

#[test]
fn snapshot_from_models_skips_view_backed_schemas() {
    let snap = SchemaSnapshot::from_models(&[Post::SCHEMA, BigPost::SCHEMA]);
    let names: Vec<&str> = snap.tables.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"vmk_post"),
        "table-backed model must be in the snapshot: {names:?}"
    );
    assert!(
        !names.contains(&"vmk_big_post"),
        "view-backed model must NOT land in the snapshot (migrate would emit CREATE TABLE against the view): {names:?}"
    );
}
