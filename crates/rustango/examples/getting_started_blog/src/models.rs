//! Project models — every #[derive(Model)] lives here.
//!
//! Adding a struct here makes it admin-visible automatically: the
//! macro populates the `inventory` registry that
//! `rustango::admin::router(pool)` walks.

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "item", display = "name")]
// Schema columns exist for the table definition and the admin; this
// tutorial's own code never reads them back, which is what `dead_code`
// measures. Allowed rather than removed — dropping them would gut the
// example the docs walk through.
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
    pub active: bool,
}
