//! #1273 regression: `#[derive(ViewSet)]`'s `router()` must not name a
//! backend-specific pool, so a derive compiles on ANY single backend.
//! It previously named `sqlx::PgPool` and broke every sqlite/mysql-only
//! project. One `router()` fn per dialect, each cfg'd to its feature so
//! it is exercised in that backend's build (the sqlite/mysql arms are
//! the ones that regress without postgres present).

#![cfg(feature = "admin")]

use rustango::sql::Auto;
use rustango::{Model, ViewSet};

#[derive(Model, Clone)]
#[rustango(table = "vs_pool_post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(ViewSet)]
#[viewset(model = Post)]
pub struct PostViewSet;

// Compile-only: router() accepts each backend's pool via Into<Pool>.
#[cfg(feature = "postgres")]
#[allow(dead_code)]
fn _pg(pool: rustango::sql::sqlx::PgPool) -> axum::Router {
    PostViewSet::router("/api/posts", pool)
}

#[cfg(feature = "sqlite")]
#[allow(dead_code)]
fn _sqlite(pool: rustango::sql::sqlx::SqlitePool) -> axum::Router {
    PostViewSet::router("/api/posts", pool)
}

#[cfg(feature = "mysql")]
#[allow(dead_code)]
fn _mysql(pool: rustango::sql::sqlx::MySqlPool) -> axum::Router {
    PostViewSet::router("/api/posts", pool)
}
