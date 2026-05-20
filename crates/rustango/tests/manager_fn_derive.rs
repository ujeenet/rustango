//! `#[rustango(manager_fn = "...")]` — custom-named QuerySet accessor.
//! Closes #289 / T2.6.
//!
//! Pins:
//!   1. The default `Self::objects()` accessor stays alive.
//!   2. Each `manager_fn = "name"` adds an extra accessor that
//!      returns the same `QuerySet<Self>` shape.
//!   3. Multiple `manager_fn` attrs accumulate.
//!   4. The extra accessors resolve `impl <ManagerExt> for QuerySet<Self>`
//!      methods identically — chains compose.

use rustango::core::Column as _;
use rustango::query::QuerySet;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfn_post")]
#[rustango(manager(ext = "PostManagerExt"))]
#[rustango(manager_fn = "active")]
#[rustango(manager_fn = "archived")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 1)]
    status: String,
}

trait PostManagerShortcuts: Sized {
    fn published(self) -> Self;
}

impl PostManagerShortcuts for QuerySet<Post> {
    fn published(self) -> Self {
        self.where_(Post::status.eq("p"))
    }
}

#[test]
fn default_objects_accessor_still_works() {
    let q = Post::objects().compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.contains("FROM \"mfn_post\""),
        "default accessor must work, got: {sql}"
    );
}

#[test]
fn manager_fn_active_accessor_returns_fresh_queryset() {
    let q = Post::active().compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.contains("FROM \"mfn_post\""),
        "active() accessor must produce a SELECT against the model's table: {sql}"
    );
}

#[test]
fn manager_fn_archived_accessor_also_works() {
    let q = Post::archived().compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(sql.contains("FROM \"mfn_post\""));
}

#[test]
fn extra_accessor_chains_with_extension_trait_methods() {
    // The whole point: `Post::active().published()` should chain
    // through the `impl PostManagerShortcuts for QuerySet<Post>`
    // methods identically to `Post::objects().published()`.
    let via_objects = Post::objects().published().compile().unwrap();
    let via_active = Post::active().published().compile().unwrap();
    let sql_objects = Postgres.compile_select(&via_objects).unwrap().sql;
    let sql_active = Postgres.compile_select(&via_active).unwrap().sql;
    assert_eq!(
        sql_objects, sql_active,
        "manager_fn accessor must produce identical SQL to objects()"
    );
}
