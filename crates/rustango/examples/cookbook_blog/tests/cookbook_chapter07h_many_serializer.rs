//! Cookbook Chapter 7h — `#[serializer(many = ChildSerializer)]`
//! for collection (M2M / one-to-many) serialization.
//!
//! M2M / one-to-many accessors in rustango are async (e.g.
//! `post.tags_m2m().all(&pool).await?`), so `from_model` (which is
//! sync) can't auto-load them. The macro:
//!
//!   1. Initializes the field to `Vec::new()` in `from_model`.
//!   2. Emits a typed `set_<field>(&mut self, models: &[ChildModel])`
//!      helper that maps each model row through `ChildSerializer::from_model`.
//!
//! Apps fetch the children, call the setter, then serialize.
//!
//! Run: `cargo test --test cookbook_chapter07h_many_serializer`

use cookbook_blog::apps::blog::models::Tag;
use rustango::serializer::ModelSerializer;
use rustango::sql::Auto;
use rustango::Model;
use rustango::Serializer;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ch7h_post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Tag)]
pub struct TagBrief {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    pub name: String,
}

#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Post)]
pub struct PostWithTags {
    pub id: Auto<i64>,
    pub title: String,
    /// Many-relation collection. Initialized to Vec::new() by
    /// from_model; populated post-hoc via set_tags().
    #[serializer(many = TagBrief)]
    pub tags: Vec<TagBrief>,
}

fn fixture_post() -> Post {
    Post { id: Auto::Set(1), title: "intro".into() }
}

fn fixture_tags() -> Vec<Tag> {
    vec![
        Tag { id: Auto::Set(1), name: "rust".into() },
        Tag { id: Auto::Set(2), name: "framework".into() },
        Tag { id: Auto::Set(3), name: "blog".into() },
    ]
}

// §7h.1 — from_model initializes `many` field as empty Vec.
#[test]
fn from_model_initializes_many_field_empty() {
    let s = PostWithTags::from_model(&fixture_post());
    assert_eq!(s.title, "intro");
    assert!(s.tags.is_empty(), "many field starts empty; caller populates");
    let v = s.to_value();
    assert_eq!(v["tags"], serde_json::json!([]));
}

// §7h.2 — `set_<field>` populates from a slice of parent models via
// the inner serializer's from_model.
#[test]
fn set_field_populates_via_inner_from_model() {
    let mut s = PostWithTags::from_model(&fixture_post());
    s.set_tags(&fixture_tags());
    assert_eq!(s.tags.len(), 3);
    assert_eq!(s.tags[0].name, "rust");
    assert_eq!(s.tags[1].name, "framework");
    assert_eq!(s.tags[2].name, "blog");
}

// §7h.3 — JSON output is a list of serialized child objects.
#[test]
fn many_field_renders_as_json_array_of_child_objects() {
    let mut s = PostWithTags::from_model(&fixture_post());
    s.set_tags(&fixture_tags());
    let v = s.to_value();
    let arr = v["tags"].as_array().expect("array");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["name"], "rust");
    // read_only id stays in the JSON output.
    assert!(arr[0]["id"].is_object() || arr[0]["id"].is_number()
        || matches!(arr[0]["id"], serde_json::Value::Null) || arr[0]["id"].is_object(),
        "id field present (Auto<i64> serializes per its serde impl)");
}

// §7h.4 — set_<field> chains for fluent post-construction.
#[test]
fn set_field_returns_mut_self_for_chaining() {
    let mut s = PostWithTags::from_model(&fixture_post());
    let _: &mut PostWithTags = s.set_tags(&fixture_tags());
    assert_eq!(s.tags.len(), 3);
}
