//! Cookbook Chapter 7g — auto-nested FK serialization
//! (`#[serializer(nested)]`).
//!
//! When a serializer field's type is itself a Serializer and the
//! source field on the model is a `ForeignKey<Parent>`, marking the
//! field with `#[serializer(nested)]` makes the macro emit the
//! `from_model` glue:
//!
//!     <ChildSerializer>::from_model(model.<source>.value().expect(...))
//!
//! The FK must be `Loaded` at the time `from_model` runs — caller
//! either calls `.get(&pool).await?` or `.select_related("...")` on
//! the parent first.
//!
//! No DB needed for this chapter — uses `ForeignKey::loaded(pk, value)`
//! to inject the parent directly.
//!
//! NOTE on the local `Author` definition: the `Model` derive emits
//! `impl LoadRelated for <FK target>`. The orphan rule blocks the
//! macro from doing that for a target type in another crate, so
//! cross-crate `ForeignKey<RemoteAuthor>` doesn't compile today.
//! For this chapter we mirror Author locally; production apps put
//! the parent + child in the same crate (`apps/blog/models.rs`)
//! which sidesteps this entirely. The cross-crate case is a
//! framework gap tracked for a later slice.
//!
//! Run: `cargo test --test cookbook_chapter07g_nested_serializer`

use rustango::serializer::ModelSerializer;
use rustango::sql::{Auto, ForeignKey};
use rustango::Model;
use rustango::Serializer;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ch7g_author")]
pub struct LocalAuthor {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub email: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ch7g_comment")]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 500)]
    pub body: String,
    /// Typed FK lazy-loadable parent.
    pub author: ForeignKey<LocalAuthor>,
}

#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = LocalAuthor)]
pub struct AuthorBrief {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    pub name: String,
}

#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Comment)]
pub struct CommentSerializer {
    pub id: Auto<i64>,
    pub body: String,
    /// Auto-nested. Default behavior: when model.author is unloaded
    /// (no select_related, no .get()), falls back to
    /// AuthorBrief::default() so the response doesn't crash.
    #[serializer(nested)]
    pub author: AuthorBrief,
}

/// Same shape but with the `strict` flag — panics when the FK is
/// unloaded. Useful in test code that wants a hard guardrail
/// against forgetting a prefetch.
#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Comment)]
pub struct CommentSerializerStrict {
    pub id: Auto<i64>,
    pub body: String,
    #[serializer(nested(strict))]
    pub author: AuthorBrief,
}

fn ada() -> LocalAuthor {
    LocalAuthor {
        id: Auto::Set(7),
        name: "ada".into(),
        email: "ada@example.com".into(),
    }
}

#[test]
fn nested_serializer_pulls_parent_via_value_when_loaded() {
    let parent = ada();
    let comment = Comment {
        id: Auto::Set(1),
        body: "first comment".into(),
        author: ForeignKey::loaded(7, parent.clone()),
    };

    let s = CommentSerializer::from_model(&comment);
    assert_eq!(s.body, "first comment");
    // The nested AuthorBrief was populated from model.author.value().
    assert_eq!(s.author.name, "ada");

    let v = s.to_value();
    assert_eq!(v["body"], "first comment");
    // JSON shape carries the nested object — DRF's nested serializer
    // result.
    assert_eq!(v["author"]["name"], "ada");
}

// §7g.2 — default (non-strict) behavior: FK unloaded → graceful
// fallback. The nested object renders as Default::default() instead
// of crashing. Production-safe when prefetches go missing.
#[test]
fn nested_serializer_falls_back_when_fk_unloaded_by_default() {
    let comment = Comment {
        id: Auto::Set(1),
        body: "no-prefetch".into(),
        author: ForeignKey::unloaded(7),
    };
    let s = CommentSerializer::from_model(&comment);
    // No panic. Author is the Default value.
    assert_eq!(s.body, "no-prefetch");
    assert_eq!(s.author.name, ""); // Default::default() for String
    let v = s.to_value();
    assert_eq!(v["body"], "no-prefetch");
    assert_eq!(v["author"]["name"], ""); // blank nested object
}

// §7g.3 — opt-in strict mode: panic when FK is unloaded. Useful for
// test code that wants to fail loudly on missing prefetches.
#[test]
#[should_panic(expected = "requires `model.author` to be loaded")]
fn nested_strict_serializer_panics_when_fk_unloaded() {
    let comment = Comment {
        id: Auto::Set(1),
        body: "bad".into(),
        author: ForeignKey::unloaded(7),
    };
    let _ = CommentSerializerStrict::from_model(&comment);
}

// §7g.4 — strict mode also works when FK IS loaded (no panic, real data).
#[test]
fn nested_strict_works_when_fk_loaded() {
    let parent = ada();
    let comment = Comment {
        id: Auto::Set(1),
        body: "loaded ok".into(),
        author: ForeignKey::loaded(7, parent),
    };
    let s = CommentSerializerStrict::from_model(&comment);
    assert_eq!(s.author.name, "ada");
}
