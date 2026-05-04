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
    /// Auto-nested via the new attr — reads model.author (a
    /// ForeignKey<LocalAuthor>), unwraps via .value(), and feeds the
    /// borrowed parent to AuthorBrief::from_model.
    #[serializer(nested)]
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

#[test]
#[should_panic(expected = "requires `model.author` to be loaded")]
fn nested_serializer_panics_when_fk_unloaded() {
    let comment = Comment {
        id: Auto::Set(1),
        body: "bad".into(),
        author: ForeignKey::unloaded(7), // .value() returns None
    };
    let _ = CommentSerializer::from_model(&comment);
}
