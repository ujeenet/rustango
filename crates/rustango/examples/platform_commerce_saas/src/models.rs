//! Project models — every #[derive(Model)] lives here.
//!
//! Adding a struct here makes it admin-visible automatically: the
//! macro populates the `inventory` registry that
//! `rustango::admin::router(pool)` walks.

use rustango::sql::Auto;
use rustango::Model;

// `#[derive(Model)]` registers this struct at *runtime* through `inventory`,
// which rustc cannot see — so fields only the framework reads look dead to it.
// Delete this line once your own code reads them.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(table = "item", display = "name")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
    pub active: bool,
}

// Tenancy registry models (Org, Operator, User) come along automatically
// with the `tenancy` feature — you don't need to redefine them here.
