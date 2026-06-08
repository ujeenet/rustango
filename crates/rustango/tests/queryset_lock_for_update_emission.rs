//! Unit test for `QuerySet::lock_for_update` — Eloquent alias of
//! `select_for_update`. Verifies both spelling produce identical
//! compiled SelectQuery state.

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "lfu_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
}

#[test]
fn lock_for_update_is_alias_of_select_for_update() {
    let q1 = Post::objects().lock_for_update().compile().unwrap();
    let q2 = Post::objects().select_for_update().compile().unwrap();
    assert_eq!(q1.lock_mode, q2.lock_mode);
    assert!(q1.lock_mode.is_some());
}
