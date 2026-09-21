//! Compile + type-check coverage for the three model-inheritance
//! shapes in Rust idioms (Issue #51). Lives alongside the
//! `inheritance` module's rustdoc — proves the patterns compile
//! end-to-end against real `#[derive(Model)]` types.
//!
//! No live DB required — all assertions are about Rust-side
//! type / method resolution.

#![cfg(feature = "postgres")]

use chrono::{DateTime, Utc};
use rustango::core::Column as _;
use rustango::query::QuerySet;
use rustango::sql::Auto;
use rustango::Model;

// ============================================================
// SHAPE 1 — Abstract base class → shared TRAIT + per-model fields
// ============================================================

/// A "Timestamped" abstract base: every child gets
/// `created_at` / `updated_at`. In Rust we declare the BEHAVIOR
/// as a trait; field declarations live on each model.
pub trait Timestamped {
    fn created_at(&self) -> DateTime<Utc>;
    fn updated_at(&self) -> DateTime<Utc>;
}

#[derive(Model, Debug)]
#[rustango(table = "inh_article")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Timestamped for Article {
    fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }
}

#[derive(Model, Debug)]
#[rustango(table = "inh_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub article_id: i64,
    #[rustango(max_length = 500)]
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Timestamped for Comment {
    fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }
}

/// Generic helper that operates on any `Timestamped` model — the
/// place shared behaviour lives instead of a base class. Write once,
/// reuse across every child.
fn age_in_seconds<T: Timestamped>(item: &T, now: DateTime<Utc>) -> i64 {
    now.signed_duration_since(item.created_at()).num_seconds()
}

#[test]
fn abstract_base_via_trait_dispatches_across_child_models() {
    // Both Article and Comment share the `Timestamped` behavior
    // via trait dispatch, with no shared table and no base type.
    let t = "2026-01-15T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let a = Article {
        id: Auto::Set(1),
        title: "Hello".into(),
        created_at: t,
        updated_at: t,
    };
    let c = Comment {
        id: Auto::Set(1),
        article_id: 1,
        body: "Nice post".into(),
        created_at: t,
        updated_at: t,
    };
    let now = "2026-01-15T12:01:00Z".parse::<DateTime<Utc>>().unwrap();
    assert_eq!(age_in_seconds(&a, now), 60);
    assert_eq!(age_in_seconds(&c, now), 60);
}

// ============================================================
// SHAPE 2 — Multi-table inheritance → explicit OneToOne FK
// ============================================================

/// "Place IS-A entity with a name + address." The would-be base
/// class is just a regular Model with its own table.
#[derive(Model, Debug)]
#[rustango(table = "inh_place")]
#[allow(dead_code)]
pub struct Place {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub address: String,
}

/// "Restaurant IS-A Place." The OneToOne FK to Place gives the
/// multi-table inheritance shape — Restaurant has its
/// own table, but every Restaurant row corresponds to exactly one
/// Place row.
#[derive(Model, Debug)]
#[rustango(table = "inh_restaurant")]
#[allow(dead_code)]
pub struct Restaurant {
    /// FK back to Place — the OneToOne is declared, never implicit.
    #[rustango(primary_key)]
    #[rustango(o2o = "inh_place", on = "id")]
    pub place_id: i64,
    pub serves_hot_dogs: bool,
}

#[test]
fn multi_table_inheritance_via_one_to_one_fk_compiles() {
    // Type-check: the relation is wired correctly through the
    // schema. The behavior pin lives in the OneToOne tests
    // elsewhere — here we just confirm the inheritance shape
    // compiles against `#[derive(Model)]`.
    let _r = Restaurant {
        place_id: 7,
        serves_hot_dogs: true,
    };
    // Both QuerySets are usable independently: query either table
    // on its own, and hop across the OneToOne when you need to.
    let _: QuerySet<Restaurant> = Restaurant::objects();
    let _: QuerySet<Place> = Place::objects();
}

// ============================================================
// SHAPE 3 — Proxy model → extension trait on QuerySet<T>
// ============================================================

#[derive(Model, Debug)]
#[rustango(table = "inh_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub published: bool,
}

/// Proxy-style "view" — a different Manager personality on the same
/// table, spelled as an extension trait on `QuerySet<Post>` rather
/// than a second model type.
trait PublishedPostExt: Sized {
    fn only_published(self) -> Self;
}

impl PublishedPostExt for QuerySet<Post> {
    fn only_published(self) -> Self {
        self.where_(Post::published.eq(true))
    }
}

#[test]
fn proxy_model_via_extension_trait_chains_with_framework_methods() {
    // Pin: a "proxy-style" extension method composes with the
    // framework's built-in QuerySet methods in one chain.
    let _chain: QuerySet<Post> = Post::objects()
        .only_published() // proxy-style
        .limit(20); // framework
}
