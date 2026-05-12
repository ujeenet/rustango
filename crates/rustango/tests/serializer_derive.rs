#![cfg(feature = "serializer")]
//! Unit tests for #[derive(Serializer)] — no database required.

use rustango::serializer::ModelSerializer;
use rustango::Serializer;

// ------------------------------------------------------------------ Fixtures

#[derive(rustango::Model, Clone)]
#[rustango(table = "posts")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    pub title: String,
    pub body: String,
    pub view_count: i64,
}

fn post(id: i64, title: &str, body: &str, views: i64) -> Post {
    Post {
        id: rustango::sql::Auto::Set(id),
        title: title.to_owned(),
        body: body.to_owned(),
        view_count: views,
    }
}

// ------------------------------------------------------------------ Basic serializer

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
struct PostSerializer {
    pub title: String,
    pub body: String,
}

#[test]
fn from_model_copies_named_fields() {
    let p = post(1, "Hello", "World", 42);
    let s = PostSerializer::from_model(&p);
    assert_eq!(s.title, "Hello");
    assert_eq!(s.body, "World");
}

#[test]
fn to_value_includes_all_fields() {
    let p = post(1, "Hello", "World", 5);
    let s = PostSerializer::from_model(&p);
    let v = s.to_value();
    assert_eq!(v["title"], "Hello");
    assert_eq!(v["body"], "World");
}

#[test]
fn writable_fields_lists_all_fields_when_no_attrs() {
    let wf = PostSerializer::writable_fields();
    assert!(wf.contains(&"title"));
    assert!(wf.contains(&"body"));
}

#[test]
fn many_to_value_returns_array() {
    let posts = vec![post(1, "A", "aa", 0), post(2, "B", "bb", 0)];
    let v = PostSerializer::many_to_value(&posts);
    assert!(v.is_array());
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(v[0]["title"], "A");
    assert_eq!(v[1]["title"], "B");
}

// ------------------------------------------------------------------ read_only

#[derive(Serializer, Default)]
#[serializer(model = Post)]
struct PostWithReadOnly {
    pub title: String,
    #[serializer(read_only)]
    pub view_count: i64,
}

#[test]
fn read_only_field_is_included_in_output() {
    let p = post(1, "Hi", "body", 99);
    let s = PostWithReadOnly::from_model(&p);
    assert_eq!(s.view_count, 99);
    let v = s.to_value();
    assert_eq!(v["view_count"], 99);
}

#[test]
fn read_only_field_excluded_from_writable_fields() {
    let wf = PostWithReadOnly::writable_fields();
    assert!(wf.contains(&"title"));
    assert!(!wf.contains(&"view_count"));
}

// ------------------------------------------------------------------ write_only

#[derive(Serializer, Default)]
#[serializer(model = Post)]
struct PostWithWriteOnly {
    pub title: String,
    #[serializer(write_only)]
    pub body: String,
}

#[test]
fn write_only_field_excluded_from_json() {
    let p = post(1, "Hi", "secret body", 0);
    let s = PostWithWriteOnly::from_model(&p);
    let v = s.to_value();
    assert_eq!(v["title"], "Hi");
    assert!(v.get("body").is_none());
}

#[test]
fn write_only_field_in_writable_fields() {
    let wf = PostWithWriteOnly::writable_fields();
    assert!(wf.contains(&"title"));
    assert!(wf.contains(&"body"));
}

// ------------------------------------------------------------------ source

#[derive(Serializer, Default)]
#[serializer(model = Post)]
struct PostRenamed {
    pub title: String,
    #[serializer(source = "body")]
    pub content: String,
}

#[test]
fn source_reads_from_named_model_field() {
    let p = post(1, "Title", "The body text", 0);
    let s = PostRenamed::from_model(&p);
    assert_eq!(s.content, "The body text");
    let v = s.to_value();
    assert_eq!(v["content"], "The body text");
}

#[test]
fn source_field_name_used_in_writable_fields() {
    let wf = PostRenamed::writable_fields();
    assert!(wf.contains(&"content")); // serializer field name, not model field name
}

// ------------------------------------------------------------------ skip

#[derive(Serializer, Default)]
#[serializer(model = Post)]
struct PostWithSkip {
    pub title: String,
    #[serializer(skip)]
    pub computed: String,
}

#[test]
fn skip_field_defaults_in_from_model_but_appears_in_output() {
    let p = post(1, "T", "b", 0);
    let mut s = PostWithSkip::from_model(&p);
    // from_model sets skip field to default
    assert_eq!(s.computed, "");
    // user sets it manually
    s.computed = "my_value".to_owned();
    let v = s.to_value();
    assert_eq!(v["computed"], "my_value");
}

#[test]
fn skip_field_excluded_from_writable_fields() {
    let wf = PostWithSkip::writable_fields();
    assert!(wf.contains(&"title"));
    assert!(!wf.contains(&"computed"));
}

// ------------------------------------------------------------------ validate (inherent method)

#[derive(Serializer, Default)]
#[serializer(model = Post)]
struct ValidatedSerializer {
    pub title: String,
}

impl ValidatedSerializer {
    pub fn validate(&self) -> Result<(), rustango::forms::FormErrors> {
        let mut errors = rustango::forms::FormErrors::default();
        if self.title.is_empty() {
            errors.add("title", "title cannot be empty");
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[test]
fn custom_validate_fires_on_empty_title() {
    let s = ValidatedSerializer {
        title: String::new(),
    };
    assert!(s.validate().is_err());
}

#[test]
fn custom_validate_passes_with_title() {
    let s = ValidatedSerializer {
        title: "Hi".to_owned(),
    };
    assert!(s.validate().is_ok());
}

// ============================================================ #[cfg(feature = "openapi")] auto-derive
//
// `#[derive(Serializer)]` also emits `impl OpenApiSchema` when the
// `openapi` feature is on — verify the produced schemas match the
// declared field types.

#[cfg(feature = "openapi")]
mod openapi_auto_derive {
    use super::*;
    use rustango::openapi::{OpenApiSchema, Schema};
    use serde_json::Value;

    fn schema_value<S: OpenApiSchema>() -> Value {
        serde_json::to_value(S::openapi_schema()).unwrap()
    }

    #[test]
    fn primitive_fields_get_correct_types_and_required() {
        let v = schema_value::<PostSerializer>();
        assert_eq!(v["type"], "object");
        assert_eq!(v["properties"]["title"]["type"], "string");
        assert_eq!(v["properties"]["body"]["type"], "string");
        // Both fields are non-Option, so both required.
        let req = v["required"].as_array().unwrap();
        assert!(req.iter().any(|s| s == "title"));
        assert!(req.iter().any(|s| s == "body"));
    }

    #[test]
    fn read_only_field_appears_in_schema() {
        let v = schema_value::<PostWithReadOnly>();
        assert_eq!(v["properties"]["title"]["type"], "string");
        assert_eq!(v["properties"]["view_count"]["type"], "integer");
        assert_eq!(v["properties"]["view_count"]["format"], "int64");
    }

    #[derive(Serializer, Default)]
    #[serializer(model = Post)]
    struct WithOptional {
        pub title: String,
        // `skip` — Post doesn't have an Option<String> field; this just
        // exercises the macro's Option<T> → nullable + not-required handling.
        #[serializer(skip)]
        pub maybe_body: Option<String>,
    }

    #[test]
    fn option_field_is_nullable_and_not_required() {
        let v = schema_value::<WithOptional>();
        assert_eq!(v["properties"]["title"]["type"], "string");
        assert_eq!(v["properties"]["maybe_body"]["type"], "string");
        assert_eq!(v["properties"]["maybe_body"]["nullable"], true);
        // `required` should contain `title` but not `maybe_body`.
        let req: Vec<&str> = v["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(req.contains(&"title"));
        assert!(!req.contains(&"maybe_body"));
    }

    #[derive(Serializer, Default)]
    #[serializer(model = Post)]
    struct WithWriteOnly {
        pub title: String,
        #[serializer(write_only)]
        pub secret: String,
    }

    #[test]
    fn write_only_field_excluded_from_schema() {
        let v = schema_value::<WithWriteOnly>();
        assert!(v["properties"].get("title").is_some());
        // write_only is excluded from the JSON output → also excluded from the schema.
        assert!(v["properties"].get("secret").is_none());
        let req: Vec<&str> = v["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(!req.contains(&"secret"));
    }

    #[test]
    fn schema_for_serializer_helper_returns_the_same_schema() {
        let direct = serde_json::to_value(PostSerializer::openapi_schema()).unwrap();
        let via_helper = serde_json::to_value(Schema::for_serializer::<PostSerializer>()).unwrap();
        assert_eq!(direct, via_helper);
    }
}
