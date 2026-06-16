//! Compile-smoke for `#[derive(ViewSet)] #[viewset(serializer = S)]`.
//!
//! The derive's `router(prefix, PgPool)` is the PG static-pool entry
//! point, so this only exercises that the macro emits a valid
//! `.serializer::<S>()` call in the generated builder chain — it does
//! not run (no live DB). Runtime rendering is covered tri-dialect by
//! `viewset_serializer_render_sqlite_live.rs`. This test exists so a
//! regression in `expand_viewset`'s serializer emit trips the
//! `--all-features` build that CI runs.

#![cfg(all(feature = "postgres", feature = "serializer"))]

use rustango::sql::Auto;
use rustango::{Model, Serializer, ViewSet};

#[derive(Model, Clone)]
#[rustango(table = "vs_derive_ser_post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 500)]
    pub body: String,
}

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
#[allow(dead_code)] // body is write-only by design.
pub struct PostSerializer {
    pub title: String,
    #[serializer(method = "excerpt")]
    pub excerpt: String,
    #[serializer(write_only)]
    pub body: String,
}

impl PostSerializer {
    fn excerpt(p: &Post) -> String {
        p.body.chars().take(10).collect()
    }
}

#[derive(ViewSet)]
#[viewset(
    model = Post,
    serializer = PostSerializer,
    ordering = "-id",
    page_size = 25,
)]
pub struct PostViewSet;

/// Compile-only: the derive must emit a `router(prefix, PgPool)` that
/// wires `.serializer::<PostSerializer>()`. Never called.
#[allow(dead_code)]
fn _router_signature_compiles(pool: rustango::sql::sqlx::PgPool) -> axum::Router {
    PostViewSet::router("/api/posts", pool)
}
