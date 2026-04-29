//! Project layout — `models.rs` (Django shape).
//!
//! All `#[derive(Model)]` types live here. Each derive populates the
//! `inventory` registry, so any `models::*` you import into the
//! `main` binary (or pull via `use models::*` in `urls.rs`) becomes
//! visible to the auto-admin without further wiring.
//!
//! Convention: one struct per row table. Re-export each model so
//! sibling modules can reach them with a flat `use crate::models::Post;`.

use rustango::sql::{Auto, ForeignKey};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "layout_user", display = "username")]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub username: String,
    pub active: bool,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "layout_post", display = "title")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 128)]
    pub title: String,
    pub author: ForeignKey<User>,
    pub published: bool,
}
