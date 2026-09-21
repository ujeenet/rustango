//! Model-inheritance patterns: how to get Django's abstract,
//! multi-table and proxy models in Rust.
//!
//! Rust has no class inheritance, but each Django shape has a
//! counterpart the framework already supports.
//!
//! ### 1. Abstract base classes → traits
//!
//! Put the shared methods in a trait and `impl` it on each model.
//! Rust cannot share field declarations, so repeat those per model.
//!
//! ```ignore
//! use chrono::{DateTime, Utc};
//!
//! /// Shared "timestamps" behavior. Implemented on every model that
//! /// wants the `created_at` / `updated_at` audit pair.
//! pub trait Timestamped {
//!     fn created_at(&self) -> DateTime<Utc>;
//!     fn updated_at(&self) -> DateTime<Utc>;
//! }
//!
//! #[derive(rustango::Model)]
//! pub struct Article {
//!     #[rustango(primary_key)]
//!     pub id: rustango::core::Auto<i64>,
//!     #[rustango(max_length = 200)]
//!     pub title: String,
//!     pub created_at: DateTime<Utc>,
//!     pub updated_at: DateTime<Utc>,
//! }
//!
//! impl Timestamped for Article {
//!     fn created_at(&self) -> DateTime<Utc> { self.created_at }
//!     fn updated_at(&self) -> DateTime<Utc> { self.updated_at }
//! }
//! ```
//!
//! Any helper that takes `<T: Timestamped>` then works for every
//! model that implements it.
//!
//! ### 2. Multi-table inheritance → an explicit one-to-one FK
//!
//! Use `#[rustango(o2o = "parent_table", on = "parent_id")]`:
//!
//! ```ignore
//! #[derive(rustango::Model)]
//! #[rustango(table = "place")]
//! pub struct Place {
//!     #[rustango(primary_key)]
//!     pub id: rustango::core::Auto<i64>,
//!     #[rustango(max_length = 200)]
//!     pub name: String,
//!     #[rustango(max_length = 200)]
//!     pub address: String,
//! }
//!
//! #[derive(rustango::Model)]
//! #[rustango(table = "restaurant")]
//! pub struct Restaurant {
//!     /// One-to-one back to Place — semantically "Restaurant IS-A Place".
//!     #[rustango(primary_key)]
//!     #[rustango(o2o = "place", on = "id")]
//!     pub place_id: i64,
//!     pub serves_hot_dogs: bool,
//! }
//! ```
//!
//! To read parent and child fields together, use
//! `Restaurant::objects().select_related("place_id")`.
//!
//! ### 3. Proxy models → extension trait
//!
//! A proxy is one table with several sets of helper methods. In Rust
//! that is an extension trait on `QuerySet<T>`. See [`crate::manager`]
//! for the full pattern.
//!
//! ```ignore
//! pub trait PublishedArticleExt: Sized {
//!     fn only_published(self) -> Self;
//! }
//! impl PublishedArticleExt for rustango::query::QuerySet<Article> {
//!     fn only_published(self) -> Self {
//!         use rustango::core::Column as _;
//!         self.where_(Article::published.eq(true))
//!     }
//! }
//!
//! // Acts as a "PublishedArticle" proxy:
//! Article::objects().only_published().fetch(&pool).await?;
//! ```
//!
//! ## Summary
//!
//! | Django shape | Rust idiom |
//! |--------------|-----------|
//! | Abstract base class | trait, plus the fields on each model |
//! | Multi-table inheritance | explicit `#[rustango(o2o)]` FK |
//! | Proxy model | extension trait on `QuerySet<T>` |
//!
//! There is no `#[rustango(abstract)]` attribute. Copying parent
//! fields at macro time would need the `Model` derive to read another
//! struct, and traits are more flexible: a model can implement many
//! of them, but could only have one abstract parent.

// Doc-only module; the patterns live in `tests/inheritance_patterns.rs`.
