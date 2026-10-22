//! Serializer `nested` / `slug` over a nullable `Option<ForeignKey<T>>`,
//! and a Form `Option<bool>`. Neither derive compiled for these.

#![cfg(all(feature = "serializer", feature = "forms"))]

use std::collections::HashMap;

use rustango::forms::Form as _;
use rustango::serializer::ModelSerializer;
use rustango::sql::{Auto, ForeignKey};
use rustango::{Form, Serializer};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "opt_rel_author")]
pub struct OptAuthor {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "opt_rel_note")]
pub struct OptNote {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub body: String,
    pub author: Option<ForeignKey<OptAuthor>>,
}

#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = OptAuthor)]
struct AuthorBrief {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    pub name: String,
}

#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = OptNote)]
struct NoteSerializer {
    pub body: String,
    #[serializer(nested)]
    pub author: Option<AuthorBrief>,
    #[serializer(slug = "name", source = "author")]
    pub author_name: Option<String>,
}

#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = OptNote)]
struct StrictNoteSerializer {
    pub body: String,
    #[serializer(nested(strict))]
    pub author: Option<AuthorBrief>,
}

fn note(author: Option<ForeignKey<OptAuthor>>) -> OptNote {
    OptNote {
        id: Auto::Set(1),
        body: "hi".into(),
        author,
    }
}

#[test]
fn nested_and_slug_read_a_loaded_nullable_relation() {
    let ada = OptAuthor {
        id: Auto::Set(7),
        name: "ada".into(),
    };
    let s = NoteSerializer::from_model(&note(Some(ForeignKey::loaded(7, ada))));
    assert_eq!(s.author.unwrap().name, "ada");
    assert_eq!(s.author_name.as_deref(), Some("ada"));
}

#[test]
fn nested_and_slug_are_null_on_a_null_relation() {
    let v = NoteSerializer::from_model(&note(None)).to_value();
    assert_eq!(v["author"], serde_json::Value::Null);
    assert_eq!(v["author_name"], serde_json::Value::Null);
}

#[test]
fn nested_and_slug_default_on_unloaded() {
    let s = NoteSerializer::from_model(&note(Some(ForeignKey::unloaded(7))));
    assert_eq!(s.author.unwrap().name, "");
    assert_eq!(s.author_name.as_deref(), Some(""));
}

#[test]
fn strict_nested_renders_a_null_row_in_a_list_as_null() {
    let ada = OptAuthor {
        id: Auto::Set(7),
        name: "ada".into(),
    };
    let rows = [note(Some(ForeignKey::loaded(7, ada))), note(None)];
    let v = StrictNoteSerializer::many_to_value(&rows);
    assert_eq!(v[0]["author"]["name"], "ada");
    assert_eq!(v[1]["author"], serde_json::Value::Null);
}

#[test]
#[should_panic(expected = "requires `model.author` to be loaded")]
fn strict_nested_panics_on_an_unloaded_relation() {
    let _ = StrictNoteSerializer::from_model(&note(Some(ForeignKey::unloaded(7))));
}

#[derive(Form, Debug, PartialEq)]
pub struct FilterForm {
    pub published: Option<bool>,
}

fn parse(pairs: &[(&str, &str)]) -> FilterForm {
    let data: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    FilterForm::parse(&data).expect("parse")
}

#[test]
fn optional_bool_is_none_when_absent_or_blank() {
    assert_eq!(parse(&[]).published, None);
    assert_eq!(parse(&[("published", "")]).published, None);
    assert_eq!(parse(&[("published", "on")]).published, Some(true));
    assert_eq!(parse(&[("published", "false")]).published, Some(false));
}

#[test]
fn optional_bool_reads_every_falsy_and_truthy_word_in_any_case() {
    for word in ["0", "off", "no", "false", "FALSE", "Off", "NO"] {
        assert_eq!(parse(&[("published", word)]).published, Some(false), "{word}");
    }
    for word in ["1", "on", "yes", "true", "TRUE", "On"] {
        assert_eq!(parse(&[("published", word)]).published, Some(true), "{word}");
    }
}
