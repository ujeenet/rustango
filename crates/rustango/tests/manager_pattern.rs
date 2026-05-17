//! Compile + type-check coverage for the `manager` extension-trait
//! pattern (Issue #52). Uses a real `#[derive(Model)]` type so the
//! chain `Article::objects().<custom>().<framework>()` is verified
//! end-to-end without needing a live database.
//!
//! No live DB required — all assertions are about type / method
//! resolution. The chained QuerySet is built but not executed.

#![cfg(feature = "postgres")]

use rustango::core::Column as _;
use rustango::query::QuerySet;
use rustango::Model;

#[derive(Model, Debug)]
#[rustango(table = "manager_pattern_article")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    published: bool,
    author_id: i64,
}

/// Canonical "custom manager" shape — Django's
/// `ArticleQuerySet.published()` / `.by_author(user)` as a Rust
/// extension trait on `QuerySet<Article>`.
trait ArticleQuerySetExt: Sized {
    fn published(self) -> Self;
    fn by_author(self, author_id: i64) -> Self;
}

impl ArticleQuerySetExt for QuerySet<Article> {
    fn published(self) -> Self {
        self.where_(Article::published.eq(true))
    }
    fn by_author(self, author_id: i64) -> Self {
        self.where_(Article::author_id.eq(author_id))
    }
}

#[test]
fn extension_trait_chains_with_framework_methods() {
    // Pin: a custom extension method composes with the framework's
    // built-in QuerySet methods (.where_, .limit, .order_by, etc.)
    // in either order, and the final type is still QuerySet<Article>.
    let _chain: QuerySet<Article> = Article::objects()
        .published() // custom
        .by_author(7) // custom
        .order_by(&[("id", false)]) // framework: id DESC
        .limit(10);
}

#[test]
fn extension_trait_composes_with_where_after_custom_method() {
    // Pin: framework `.where_()` can be applied AFTER a custom
    // shortcut without method-resolution conflict.
    let _chain: QuerySet<Article> = Article::objects()
        .published()
        .where_(Article::author_id.eq(42));
}

#[test]
fn multiple_extension_traits_can_coexist() {
    // Define a second trait and verify both can be brought into
    // scope simultaneously without conflict — Django supports
    // multiple Managers per model (`objects` + `published_manager`).
    trait ArchivedQuerySetExt: Sized {
        fn archived(self) -> Self;
    }
    impl ArchivedQuerySetExt for QuerySet<Article> {
        fn archived(self) -> Self {
            // Stub: in a real impl this'd filter on a
            // `published = false` or `deleted_at IS NOT NULL`
            // column. Pinning method shape here.
            self
        }
    }
    let _chain: QuerySet<Article> = Article::objects().published().archived();
}

/// "Manager-style" accessor — Django's `Article.published_manager =
/// PublishedManager()` adapted to a free function on `impl Article`.
/// This is the alternative shape when you want a named accessor
/// that always pre-applies a filter (vs an extension trait that
/// chains on demand).
impl Article {
    fn published_objects() -> QuerySet<Article> {
        Article::objects().where_(Article::published.eq(true))
    }
}

#[test]
fn impl_method_manager_returns_pre_filtered_queryset() {
    // Pin: an `impl Article { pub fn published_objects() ... }`
    // shape returns a QuerySet that's already pre-filtered, and
    // every framework method composes from there.
    let _chain: QuerySet<Article> = Article::published_objects()
        .order_by(&[("title", true)])
        .limit(20);
}
