//! `makemigrations` refuses one `through` table with two shapes as an error (#2000).

use rustango::migrate::{make_migrations, MigrateError};
use rustango::Model;

#[derive(Model)]
#[rustango(
    table = "jc_post",
    m2m(
        name = "tags",
        to = "jc_tag",
        through = "jc_shared",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model)]
#[rustango(
    table = "jc_note",
    m2m(
        name = "tags",
        to = "jc_tag",
        through = "jc_shared",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn a_second_junction_shape_is_a_validation_error() {
    let dir = std::env::temp_dir().join(format!("rustango_jc_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    match make_migrations(&dir, None) {
        Err(MigrateError::Validation(m)) => assert!(m.contains("`jc_shared`"), "{m}"),
        other => panic!("expected Validation, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
