//! `#[rgo(...)]` is a second name for `#[rustango(...)]`: the same model
//! spelled either way must produce the same schema.

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rgo_alias_note", display = "title", app = "rgo_alias")]
#[rustango(index("title, view_count"))]
#[allow(dead_code)]
pub struct LongNote {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80, unique, help_text = "shown in lists")]
    pub title: String,
    #[rustango(column = "view_count", default = "0", min = 0)]
    pub views: i64,
    #[rustango(max_length = 16, choices = "draft, live")]
    pub state: String,
}

#[derive(Model, Debug, Clone)]
#[rgo(table = "rgo_alias_note", display = "title", app = "rgo_alias")]
#[rgo(index("title, view_count"))]
#[allow(dead_code)]
pub struct ShortNote {
    #[rgo(primary_key)]
    pub id: Auto<i64>,
    #[rgo(max_length = 80, unique, help_text = "shown in lists")]
    pub title: String,
    #[rgo(column = "view_count", default = "0", min = 0)]
    pub views: i64,
    #[rgo(max_length = 16, choices = "draft, live")]
    pub state: String,
}

#[test]
fn rgo_and_rustango_build_the_same_schema() {
    use rustango::core::Model as _;
    let long = format!("{:?}", LongNote::SCHEMA).replace("LongNote", "Note");
    let short = format!("{:?}", ShortNote::SCHEMA).replace("ShortNote", "Note");
    assert_eq!(long, short);
    // The attributes were read, not ignored under both names.
    assert_eq!(ShortNote::SCHEMA.table, "rgo_alias_note");
    let views = ShortNote::SCHEMA.field("views").expect("views field");
    assert_eq!(views.column, "view_count");
    assert_eq!(
        ShortNote::SCHEMA.field("title").unwrap().max_length,
        Some(80)
    );
}
