//! [JSON:API](https://jsonapi.org) v1.1 response shape adapter.
//!
//! Converts a flat `serde_json::Value` (typical output of
//! [`crate::serializer`]) into the JSON:API envelope:
//!
//! ```json
//! {
//!   "data": {
//!     "type": "posts",
//!     "id":   "42",
//!     "attributes": { "title": "...", "body": "..." }
//!   }
//! }
//! ```
//!
//! Or for collections:
//!
//! ```json
//! {
//!   "data": [
//!     { "type": "posts", "id": "1", "attributes": {...} },
//!     { "type": "posts", "id": "2", "attributes": {...} }
//!   ],
//!   "meta": { "count": 2 }
//! }
//! ```
//!
//! Most apps wire it as a thin handler-side helper:
//!
//! ```ignore
//! use rustango::jsonapi::{to_resource, to_collection};
//!
//! async fn show_post(...) -> Json<Value> {
//!     let s = PostSerializer::from_model(&post);
//!     Json(to_resource("posts", &s.id.to_string(), s.to_value()))
//! }
//!
//! async fn list_posts(...) -> Json<Value> {
//!     let posts: Vec<PostSerializer> = PostSerializer::many(&rows);
//!     let docs: Vec<_> = posts.iter()
//!         .map(|p| (p.id.to_string(), p.to_value()))
//!         .collect();
//!     Json(to_collection("posts", &docs))
//! }
//! ```
//!
//! ## What this does NOT cover (yet)
//!
//! - `relationships` — the spec lets you express FKs/M2Ms inline;
//!   for now, embed via `attributes` or use `included` (helpers
//!   below).
//! - Sparse fieldsets, sorting, filtering — those live in your
//!   ViewSet / handler, not this adapter.
//! - Errors envelope — use [`crate::problem_details`] for RFC 7807
//!   (clean, widely-supported alternative).

use serde_json::{json, Map, Value};

const ATTRIBUTES_RESERVED: &[&str] = &["id", "type"];

/// Wrap a single resource into the JSON:API top-level `{"data": …}`.
/// `id` is always rendered as a string per the spec.
#[must_use]
pub fn to_resource(resource_type: &str, id: &str, attributes: Value) -> Value {
    json!({
        "data": resource_object(resource_type, id, attributes)
    })
}

/// Wrap a collection of resources. `docs` is `[(id, attributes), ...]`.
/// Adds `meta.count` for free so clients can size pagers.
#[must_use]
pub fn to_collection(resource_type: &str, docs: &[(String, Value)]) -> Value {
    let arr: Vec<Value> = docs
        .iter()
        .map(|(id, attrs)| resource_object(resource_type, id, attrs.clone()))
        .collect();
    let count = arr.len();
    json!({
        "data": arr,
        "meta": { "count": count }
    })
}

/// Produce the inner `{"type", "id", "attributes"}` object — useful
/// when you're hand-building a response with `included`/`meta`/etc.
#[must_use]
pub fn resource_object(resource_type: &str, id: &str, attributes: Value) -> Value {
    let attrs = strip_reserved(attributes);
    json!({
        "type": resource_type,
        "id":   id,
        "attributes": attrs
    })
}

/// Add `included` to an existing resource doc. The new doc is merged
/// into `doc["included"]` (creating it if absent). Per the spec,
/// `included` is an array of full resource objects — pass them
/// already-shaped via [`resource_object`].
#[must_use]
pub fn with_included(mut doc: Value, included: Vec<Value>) -> Value {
    let Some(obj) = doc.as_object_mut() else {
        return doc;
    };
    let entry = obj
        .entry("included")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(existing) = entry {
        existing.extend(included);
    }
    doc
}

/// Add an arbitrary `meta` field merged into the existing meta object.
/// Useful for pagination cursors, totals, ETags, etc.
#[must_use]
pub fn with_meta(mut doc: Value, meta: Value) -> Value {
    let Some(obj) = doc.as_object_mut() else {
        return doc;
    };
    if let Some(existing) = obj.get_mut("meta") {
        if let (Some(existing_obj), Some(new_obj)) = (existing.as_object_mut(), meta.as_object()) {
            for (k, v) in new_obj {
                existing_obj.insert(k.clone(), v.clone());
            }
            return doc;
        }
    }
    obj.insert("meta".to_owned(), meta);
    doc
}

/// Strip `id` and `type` keys from a flat attributes object — the
/// spec says these MUST live at the top level of the resource
/// object, not inside `attributes`. Anything else passes through.
fn strip_reserved(value: Value) -> Value {
    let Value::Object(map) = value else {
        return value;
    };
    let mut out = Map::with_capacity(map.len());
    for (k, v) in map {
        if !ATTRIBUTES_RESERVED.contains(&k.as_str()) {
            out.insert(k, v);
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_resource_wraps_attributes_in_data_envelope() {
        let attrs = json!({"id": 42, "title": "Hello", "body": "world"});
        let doc = to_resource("posts", "42", attrs);
        assert_eq!(doc["data"]["type"], "posts");
        assert_eq!(doc["data"]["id"], "42");
        assert_eq!(doc["data"]["attributes"]["title"], "Hello");
        assert_eq!(doc["data"]["attributes"]["body"], "world");
        // `id` is stripped from attributes per spec.
        assert!(doc["data"]["attributes"].get("id").is_none());
    }

    #[test]
    fn to_resource_strips_type_from_attributes() {
        let attrs = json!({"type": "should-be-stripped", "title": "x"});
        let doc = to_resource("posts", "1", attrs);
        assert!(doc["data"]["attributes"].get("type").is_none());
        assert_eq!(doc["data"]["type"], "posts");
    }

    #[test]
    fn id_is_always_a_string_in_the_envelope() {
        let doc = to_resource("posts", "42", json!({"title": "x"}));
        assert!(doc["data"]["id"].is_string());
        assert_eq!(doc["data"]["id"], "42");
    }

    #[test]
    fn to_collection_renders_array_with_count_meta() {
        let docs = vec![
            ("1".to_owned(), json!({"title": "A"})),
            ("2".to_owned(), json!({"title": "B"})),
        ];
        let doc = to_collection("posts", &docs);
        assert!(doc["data"].is_array());
        assert_eq!(doc["data"].as_array().unwrap().len(), 2);
        assert_eq!(doc["data"][0]["type"], "posts");
        assert_eq!(doc["data"][0]["id"], "1");
        assert_eq!(doc["data"][1]["attributes"]["title"], "B");
        assert_eq!(doc["meta"]["count"], 2);
    }

    #[test]
    fn empty_collection_renders_empty_array_and_zero_count() {
        let doc = to_collection("posts", &[]);
        assert_eq!(doc["data"].as_array().unwrap().len(), 0);
        assert_eq!(doc["meta"]["count"], 0);
    }

    #[test]
    fn resource_object_is_inner_shape_only() {
        let inner = resource_object("posts", "1", json!({"title": "X"}));
        assert_eq!(inner["type"], "posts");
        assert_eq!(inner["id"], "1");
        assert_eq!(inner["attributes"]["title"], "X");
        // No "data" wrapper — the inner shape is for hand-building.
        assert!(inner.get("data").is_none());
    }

    #[test]
    fn with_included_appends_to_array_creating_it_when_absent() {
        let doc = to_resource("posts", "1", json!({}));
        let included = vec![resource_object("authors", "7", json!({"name": "Alice"}))];
        let with = with_included(doc, included);
        assert_eq!(with["included"].as_array().unwrap().len(), 1);
        assert_eq!(with["included"][0]["type"], "authors");
        assert_eq!(with["included"][0]["id"], "7");
    }

    #[test]
    fn with_included_appends_to_existing_array() {
        let doc = json!({"data": {}, "included": [{"type":"a","id":"1","attributes":{}}]});
        let with = with_included(doc, vec![json!({"type":"b","id":"2","attributes":{}})]);
        assert_eq!(with["included"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn with_meta_merges_into_existing_meta_object() {
        let doc = to_collection("posts", &[("1".into(), json!({}))]);
        // collection already has meta.count = 1
        let with = with_meta(doc, json!({"page": 1, "page_size": 20}));
        assert_eq!(with["meta"]["count"], 1);
        assert_eq!(with["meta"]["page"], 1);
        assert_eq!(with["meta"]["page_size"], 20);
    }

    #[test]
    fn with_meta_creates_meta_when_absent() {
        let doc = to_resource("posts", "1", json!({"title":"x"}));
        let with = with_meta(doc, json!({"updated_at": "2024-01-01"}));
        assert_eq!(with["meta"]["updated_at"], "2024-01-01");
    }

    #[test]
    fn non_object_attributes_pass_through_unchanged() {
        // String / number / null aren't object-shaped, so the
        // strip-reserved pass shouldn't touch them. (Spec actually
        // requires attributes to be an object, but we don't enforce —
        // the JSON:API client will reject if the shape is wrong.)
        let doc = to_resource("counts", "1", json!(42));
        assert_eq!(doc["data"]["attributes"], 42);
    }
}
