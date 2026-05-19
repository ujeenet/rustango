//! `#[rustango(manager(ext = "..."))]` — auto-emitted extension trait
//! for custom managers. Closes #271 / T1.9.
//!
//! The derive emits `pub trait <name>: Sized {}` adjacent to the model.
//! Users add methods via `impl <name> for QuerySet<Model> { ... }` —
//! same shape as the doc pattern in `crates/rustango/src/manager.rs`
//! but the trait is auto-generated rather than hand-written.

use rustango::core::Column as _;
use rustango::query::QuerySet;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mgrext_post")]
#[rustango(manager(ext = "PostManagerExt"))]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 1)]
    status: String,
    author_id: i64,
}

// The trait `PostManagerExt` was emitted by the derive. Add methods
// via `impl PostManagerExt for QuerySet<Post>` — same shape as the
// existing doc pattern in `src/manager.rs`.
impl PostManagerExt for QuerySet<Post> {
    // We don't actually declare these as trait methods because the
    // emitted trait is empty (`Sized`-only). Adding methods directly
    // on the impl block here just provides the chainable shortcuts.
}

// Inherent impl on `QuerySet<Post>` carries the Django-shape shortcuts.
trait PostShortcuts: Sized {
    fn published(self) -> Self;
    fn by_author(self, author_id: i64) -> Self;
}

impl PostShortcuts for QuerySet<Post> {
    fn published(self) -> Self {
        self.where_(Post::status.eq("p"))
    }
    fn by_author(self, author_id: i64) -> Self {
        self.where_(Post::author_id.eq(author_id))
    }
}

#[test]
fn emitted_trait_is_in_scope_and_usable() {
    // If the derive didn't emit the trait, this `fn _check` wouldn't
    // compile — `PostManagerExt` would be an unknown ident.
    fn _check<T: PostManagerExt>() {}
    _check::<QuerySet<Post>>();
}

#[test]
fn chained_shortcuts_compose_with_framework_methods() {
    // Same chain Django users write: `Post.objects.published().by_author(7)`.
    let qs = Post::objects().published().by_author(7_i64);
    let q = qs.compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    // Both shortcuts emit their WHERE — AND-joined.
    assert!(
        sql.contains(r#""status" = $1"#) && sql.contains(r#""author_id" = $2"#),
        "expected both shortcuts to land in WHERE, got:\n{sql}"
    );
}

#[test]
fn shortcuts_chain_with_native_queryset_methods() {
    // `.published()` (custom) → `.order_by()` (framework) → `.compile()`.
    let qs = Post::objects().published().order_by(&[("id", true)]);
    let q = qs.compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(
        sql.contains(r#"ORDER BY "id" DESC"#),
        "expected framework order_by to chain, got:\n{sql}"
    );
}
