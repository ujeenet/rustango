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
#[allow(dead_code)] // `body` is intentionally write-only — checked by absence in the serializer output.
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
    #[allow(dead_code)] // `secret` is intentionally write-only — checked by absence in the serializer output.
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

// ============================================================================
// v0.44 — standalone unit-test coverage for advanced serializer attributes.
//
// The cookbook chapters exercise these end-to-end against the blog_demo
// schema; this module proves the macro expansion in isolation so a
// regression in `parse_serializer_field_attrs` / `expand_serializer`
// trips a fast, no-DB unit test instead of waiting for a cookbook
// build that needs sqlx + a live DB.
// ============================================================================

mod advanced_attrs {
    use rustango::serializer::ModelSerializer;
    use rustango::sql::{Auto, ForeignKey};
    use rustango::Serializer;

    #[derive(rustango::Model, Debug, Clone)]
    #[rustango(table = "v044_author")]
    pub struct V044Author {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 80)]
        pub name: String,
    }

    #[derive(rustango::Model, Debug, Clone)]
    #[rustango(table = "v044_comment")]
    pub struct V044Comment {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 500)]
        pub body: String,
        pub author: ForeignKey<V044Author>,
    }

    // ---- method = "fn_name" ----

    #[derive(Serializer, serde::Deserialize, Default)]
    #[serializer(model = super::Post)]
    struct PostWithMethod {
        pub title: String,
        #[serializer(method = "loud_title")]
        pub loud: String,
    }

    impl PostWithMethod {
        fn loud_title(p: &super::Post) -> String {
            p.title.to_uppercase()
        }
    }

    #[test]
    fn method_field_invokes_associated_fn_in_from_model() {
        let p = super::post(1, "hello", "body", 0);
        let s = PostWithMethod::from_model(&p);
        assert_eq!(s.title, "hello");
        assert_eq!(s.loud, "HELLO");
    }

    #[test]
    fn method_field_excluded_from_writable() {
        // method fields are computed — never accepted on write.
        let wf = PostWithMethod::writable_fields();
        assert!(wf.contains(&"title"));
        assert!(!wf.contains(&"loud"), "method field should not be writable");
    }

    // ---- nested ----

    #[derive(Serializer, serde::Deserialize, Default, Debug)]
    #[serializer(model = V044Author)]
    struct AuthorBrief {
        #[serializer(read_only)]
        pub id: Auto<i64>,
        pub name: String,
    }

    #[derive(Serializer, serde::Deserialize, Default, Debug)]
    #[serializer(model = V044Comment)]
    struct CommentSerializer {
        pub body: String,
        #[serializer(nested)]
        pub author: AuthorBrief,
    }

    fn author(id: i64, name: &str) -> V044Author {
        V044Author {
            id: Auto::Set(id),
            name: name.to_owned(),
        }
    }

    #[test]
    fn nested_pulls_parent_when_fk_is_loaded() {
        let parent = author(7, "ada");
        let comment = V044Comment {
            id: Auto::Set(1),
            body: "hi".into(),
            author: ForeignKey::loaded(7, parent),
        };
        let s = CommentSerializer::from_model(&comment);
        assert_eq!(s.body, "hi");
        assert_eq!(s.author.name, "ada");
    }

    #[test]
    fn nested_falls_back_to_default_when_fk_unloaded() {
        // Non-strict mode: production-degrades-gracefully. The author
        // field is unloaded (no select_related); the macro must NOT
        // panic.
        let comment = V044Comment {
            id: Auto::Set(1),
            body: "hi".into(),
            author: ForeignKey::unloaded(7),
        };
        let s = CommentSerializer::from_model(&comment);
        assert_eq!(s.body, "hi");
        assert_eq!(s.author.name, ""); // Default::default() name
    }

    // ---- many = ChildSerializer ----

    #[derive(Serializer, serde::Deserialize, Default, Debug)]
    #[serializer(model = V044Comment)]
    struct CommentBrief {
        pub body: String,
    }

    #[derive(Serializer, serde::Deserialize, Default, Debug)]
    #[serializer(model = V044Author)]
    struct AuthorWithComments {
        pub name: String,
        #[serializer(many = CommentBrief)]
        pub recent_comments: Vec<CommentBrief>,
    }

    #[test]
    fn many_field_initializes_to_empty_vec_in_from_model() {
        // The setter is the only way to populate `many` fields —
        // auto-load isn't possible (M2M / reverse-FK is async).
        let a = author(1, "ada");
        let s = AuthorWithComments::from_model(&a);
        assert_eq!(s.name, "ada");
        assert!(s.recent_comments.is_empty());
    }

    #[test]
    fn many_setter_populates_via_child_from_model() {
        let a = author(1, "ada");
        let mut s = AuthorWithComments::from_model(&a);
        let comments = [
            V044Comment {
                id: Auto::Set(1),
                body: "first".into(),
                author: ForeignKey::unloaded(1),
            },
            V044Comment {
                id: Auto::Set(2),
                body: "second".into(),
                author: ForeignKey::unloaded(1),
            },
        ];
        s.set_recent_comments(&comments);
        assert_eq!(s.recent_comments.len(), 2);
        assert_eq!(s.recent_comments[0].body, "first");
        assert_eq!(s.recent_comments[1].body, "second");
    }

    // ---- slug = "field_name" (v0.44) ----

    #[derive(Serializer, serde::Deserialize, Default, Debug)]
    #[serializer(model = V044Comment)]
    struct CommentWithAuthorSlug {
        pub body: String,
        // DRF SlugRelatedField: serialize the FK as a string slug
        // (here the author's `name` field) instead of an i64 PK.
        #[serializer(slug = "name", source = "author")]
        pub author_name: String,
    }

    #[test]
    fn slug_field_pulls_named_field_from_loaded_parent() {
        let parent = author(7, "ada");
        let c = V044Comment {
            id: Auto::Set(1),
            body: "hi".into(),
            author: ForeignKey::loaded(7, parent),
        };
        let s = CommentWithAuthorSlug::from_model(&c);
        assert_eq!(s.body, "hi");
        assert_eq!(s.author_name, "ada");
    }

    #[test]
    fn slug_field_falls_back_to_default_when_fk_unloaded() {
        let c = V044Comment {
            id: Auto::Set(1),
            body: "hi".into(),
            author: ForeignKey::unloaded(7),
        };
        let s = CommentWithAuthorSlug::from_model(&c);
        assert_eq!(s.author_name, "");
    }

    #[test]
    fn slug_field_excluded_from_writable() {
        let wf = CommentWithAuthorSlug::writable_fields();
        assert!(wf.contains(&"body"));
        assert!(
            !wf.contains(&"author_name"),
            "slug fields are display-only — must not be writable"
        );
    }

    // ---- validate = "fn_name" ----

    #[derive(Serializer, serde::Deserialize, Default)]
    #[serializer(model = super::Post)]
    struct ValidatedPost {
        #[serializer(validate = "title_at_least_3")]
        pub title: String,
        pub body: String,
    }

    impl ValidatedPost {
        fn title_at_least_3(t: &String) -> Result<(), String> {
            if t.chars().count() < 3 {
                Err("title must be at least 3 chars".to_owned())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn per_field_validator_passes_on_valid_input() {
        let s = ValidatedPost {
            title: "Hello".into(),
            body: "body".into(),
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn per_field_validator_collects_field_keyed_error() {
        let s = ValidatedPost {
            title: "hi".into(), // 2 chars — too short
            body: "body".into(),
        };
        let err = s.validate().expect_err("should fail");
        let title_errs = err.get("title");
        assert!(
            !title_errs.is_empty(),
            "FormErrors should carry the title key, got fields: {:?}",
            err.fields()
        );
        assert!(title_errs[0].contains("3 chars"));
    }
}
