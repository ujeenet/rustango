//! Custom Manager / QuerySet-extension pattern — Django's
//! `class PublishedManager(Manager)` and `QuerySet.as_manager()`
//! adapted to Rust. Issue #52.
//!
//! Django ships **Managers** (per-model accessors that produce
//! QuerySets) so applications can attach domain-specific shortcuts
//! to `.objects` — `Article.objects.published()`, `User.active.all()`,
//! etc. The two canonical Django idioms are:
//!
//! ```python
//! # 1. Custom Manager that overrides get_queryset:
//! class PublishedManager(models.Manager):
//!     def get_queryset(self):
//!         return super().get_queryset().filter(published=True)
//!
//! class Article(models.Model):
//!     objects = models.Manager()
//!     published = PublishedManager()
//!
//! # 2. Custom QuerySet promoted to Manager via .as_manager():
//! class ArticleQuerySet(models.QuerySet):
//!     def published(self):
//!         return self.filter(published=True)
//!     def by_author(self, user):
//!         return self.filter(author=user)
//!
//! class Article(models.Model):
//!     objects = ArticleQuerySet.as_manager()
//! ```
//!
//! ## The Rust idiom: extension traits
//!
//! Rust's **extension trait** idiom maps directly to Django's
//! `as_manager()` — and it's strictly more flexible because the
//! returned chained value is still a `QuerySet<T>`, so every existing
//! method (`.where_`, `.order_by`, `.fetch`) stays available
//! alongside the app-specific shortcuts. No subclassing, no method
//! resolution surprises.
//!
//! ```ignore
//! use rustango::core::Column;
//! use rustango::query::QuerySet;
//!
//! #[derive(rustango::Model, Debug)]
//! struct Article {
//!     #[rustango(primary_key, auto)]
//!     pub id: rustango::core::Auto<i64>,
//!     #[rustango(max_length = 200)]
//!     pub title: String,
//!     pub published: bool,
//!     pub author_id: i64,
//! }
//!
//! /// Article-specific QuerySet helpers — Django's
//! /// `ArticleQuerySet.published()` / `.by_author(user)` shape.
//! pub trait ArticleQuerySetExt: Sized {
//!     fn published(self) -> Self;
//!     fn by_author(self, author_id: i64) -> Self;
//! }
//!
//! impl ArticleQuerySetExt for QuerySet<Article> {
//!     fn published(self) -> Self {
//!         self.where_(Article::published.eq(true))
//!     }
//!     fn by_author(self, author_id: i64) -> Self {
//!         self.where_(Article::author_id.eq(author_id))
//!     }
//! }
//!
//! // Usage — chain the shortcuts with the framework's own methods:
//! async fn recent_published(pool: &rustango::sql::Pool) {
//!     let articles = Article::objects()
//!         .published()                    // custom shortcut
//!         .by_author(7)                   // another custom shortcut
//!         .order_by("-id")                // framework method
//!         .fetch(pool).await.unwrap();
//! }
//! ```
//!
//! ## Manager-style: pin a default filter
//!
//! If you want Django's `PublishedManager`-shape (a named accessor
//! that *always* applies a base filter), expose it as a free
//! function or `impl Article` method that returns a pre-filtered
//! QuerySet:
//!
//! ```ignore
//! impl Article {
//!     /// Default accessor — every Article, no filter applied.
//!     /// (Already provided by `#[derive(Model)]` as `Article::objects()`.)
//!
//!     /// Published-only accessor — Django's `Article.published`.
//!     pub fn published_objects() -> QuerySet<Article> {
//!         Article::objects().where_(Article::published.eq(true))
//!     }
//! }
//!
//! // Usage:
//! let public_articles = Article::published_objects()
//!     .order_by("-id")
//!     .fetch(&pool).await?;
//! ```
//!
//! Either shape composes with the rest of the QuerySet builder —
//! you can call any framework method (`.where_`, `.limit`,
//! `.select_related`, `.order_by`, `.fetch`) on the result.
//!
//! ## Why no `#[rustango(manager = "...")]` attribute?
//!
//! The issue's acceptance criteria suggested `#[rustango(manager =
//! "MyPublishedManager")]` as an attribute on the model that
//! redirects `.objects()` to a user-defined struct. We don't need
//! it — extension traits achieve the same effect with strictly
//! better ergonomics:
//!
//! - **No special syntax**: it's just Rust's built-in trait system.
//!   Users already know how it works.
//! - **No subclass diamond**: the returned value is still
//!   `QuerySet<Article>`, so every framework method composes with
//!   every custom method. No "ArticleQuerySet only has a subset
//!   of QuerySet" surprises.
//! - **Multiple sets coexist**: define `PublishedQuerySetExt` and
//!   `ArchivedQuerySetExt` separately; bring whichever one you
//!   need into scope per file.
//! - **Free-function shape**: when the application doesn't need a
//!   chain — just a single shortcut — write `Article::published_only()`
//!   as a static method. Cheaper than a trait.
//!
//! Adding a `manager = "..."` attribute would compile to an
//! extension trait under the hood anyway, with extra macro
//! complexity and another way to spell the same intent.

// The pattern this module documents is built from pure Rust trait
// machinery — no framework runtime to test. Worked examples live in
// `tests/manager_pattern_live.rs`, which uses a real `#[derive(Model)]`
// type to prove the shape compiles end-to-end and the chained QuerySet
// still routes through `.fetch()`.
