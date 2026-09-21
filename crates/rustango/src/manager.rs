//! How to attach your own query shortcuts to a model, the way
//! Django's custom Managers and `QuerySet.as_manager()` do.
//!
//! Django lets an app add shortcuts to `.objects`, so callers can
//! write `Article.objects.published()`:
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
//! Write an extension trait on `QuerySet<T>`. Each shortcut returns a
//! `QuerySet<T>` again, so framework methods like `.where_`,
//! `.order_by` and `.fetch` still work on the result.
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
//! ## Pin a default filter
//!
//! For a named accessor that always applies one base filter, add a
//! method on the model that returns a pre-filtered QuerySet:
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
//! Both shapes still compose with the rest of the QuerySet builder.
//!
//! There is no `#[rustango(manager = "...")]` attribute, because a
//! plain trait already does the job: it needs no new syntax, several
//! traits can coexist on one model, and the result keeps every
//! framework method.

// Doc-only module; worked examples live in `tests/manager_pattern_live.rs`.
