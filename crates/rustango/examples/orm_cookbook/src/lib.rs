//! Models backing the recipes in `docs/orm.md`. The integration test
//! `tests/orm_smoke.rs` runs representative queries from each section
//! against a real Postgres so the documented API can't drift.

use chrono::{DateTime, Utc};
use rustango::{Auto, Model};

/// The recurring `Post` the ORM cookbook builds its examples on. Carries
/// every field the doc's snippets reference (`view_count`, `is_active`,
/// `price`, `pages`, …) so each recipe compiles against a real model.
#[derive(Model, Clone, Debug)]
#[rustango(table = "cookbook_post", display = "title")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,

    pub author_id: i64,
    pub view_count: i64,
    pub is_active: bool,
    pub price: i64,
    pub pages: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,

    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Author for the join / aggregation examples.
#[derive(Model, Clone, Debug)]
#[rustango(table = "cookbook_author", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 120)]
    pub name: String,
}
