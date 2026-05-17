//! Model-inheritance patterns — Django's `Meta.abstract`, multi-table,
//! and proxy shapes adapted to Rust. Issue #51.
//!
//! Django ships three flavors of model inheritance:
//!
//! 1. **Abstract base classes** (`Meta.abstract = True`) — share
//!    fields + methods across child models. The base has no DB
//!    table; every child gets its own table with the base's fields
//!    physically copied in.
//! 2. **Multi-table inheritance** — child gets its OWN table with an
//!    implicit `OneToOne` FK back to the parent. Queries against the
//!    child JOIN to the parent.
//! 3. **Proxy models** (`Meta.proxy = True`) — same table as the
//!    parent, but a different Manager / Meta / methods.
//!
//! ## Rust mappings
//!
//! Rust doesn't have class inheritance, but every Django use case
//! for model inheritance has a clean Rust-idiomatic counterpart
//! that the framework already supports today:
//!
//! ### 1. Abstract base classes → traits + composition
//!
//! Django's abstract base is about sharing *behavior* (methods)
//! and *fields* (schema). In Rust:
//!
//! - **Share behavior**: define a trait with the shared methods,
//!   `impl` it on each model.
//! - **Share fields**: not natively possible without a proc-macro.
//!   The pragmatic shape is to either (a) `derive` a helper proc-macro
//!   per shared block, or (b) accept the duplication on the field
//!   declarations and share the BEHAVIOR via trait.
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
//! The field declarations live on each model (Rust requirement),
//! but every shared helper / query method that takes `<T: Timestamped>`
//! works polymorphically — same dispatch shape Django gets from the
//! abstract base class.
//!
//! ### 2. Multi-table inheritance → explicit OneToOne FK
//!
//! Already supported. `#[rustango(o2o = "ParentTable", on = "parent_id")]`
//! declares an implicit-OneToOne FK to the parent table:
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
//! Django's `Restaurant.objects.all()` returns rows with both place
//! and restaurant fields joined; in rustango the equivalent is
//! `Restaurant::objects().select_related("place_id")` (or fetch
//! the parent rows separately via `place_ids = restaurants.iter().map(|r| r.place_id)`).
//!
//! ### 3. Proxy models → extension trait
//!
//! Django's proxy is "different Manager / Meta on the same table."
//! The Rust shape is an extension trait on `QuerySet<T>` that adds
//! domain-specific shortcuts — fully documented in the
//! [`crate::manager`] module. Same physical table, multiple
//! "personalities."
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
//! Article::objects().only_published().fetch_pool(&pool).await?;
//! ```
//!
//! ## Summary table
//!
//! | Django shape | Rust idiom | Framework support |
//! |--------------|-----------|-------------------|
//! | Abstract base class | trait + per-model field declarations | trait dispatch (Rust built-in) |
//! | Multi-table inheritance | explicit `#[rustango(o2o)]` FK | shipped |
//! | Proxy model | extension trait on `QuerySet<T>` | see [`crate::manager`] |
//!
//! ## Why no `#[rustango(abstract)]` proc-macro attribute?
//!
//! True field-inlining (parent fields physically copied into child
//! at macro time) would need the `Model` derive to consult a
//! parent struct's field list. Doable but a substantial
//! proc-macro lift, and the trait-based "share behavior" approach
//! is strictly more flexible — multiple traits can layer onto
//! one struct, where a single abstract parent can't.
//!
//! File a follow-up if your project repeatedly hand-copies the
//! same 5+ fields across N models — at that point a proc-macro
//! pays for itself.

// Doc-only module; the patterns this documents are exercised in
// `tests/inheritance_patterns.rs`.
