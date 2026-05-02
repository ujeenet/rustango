//! rustango — a Django-inspired ORM + admin + multi-tenancy for Rust.
//!
//! ```ignore
//! [dependencies]
//! rustango = { version = "0.7", features = ["tenancy"] }
//! ```
//!
//! Out of the box (`default = ["postgres", "admin"]`) you get the
//! ORM, the migration runner, and the auto-admin. Add `"tenancy"`
//! for the multi-tenant resolver / pools / per-tenant auth pieces.
//! Drop `default-features` for the bare ORM (no axum, no Tera).
//!
//! See the workspace [README](https://github.com/ujeenet/rustango)
//! for the full feature matrix and the Django-shape project layout.

// Lets `::rustango::core::Model` (emitted by the proc-macro) and
// `rustango::sql::Auto<i64>` (used in tenancy source code carried
// over from the pre-collapse layout) resolve to ourselves without
// rewriting either.
extern crate self as rustango;

pub mod audit;
pub mod core;
pub mod migrate;
pub mod query;
pub mod sql;

#[cfg(feature = "admin")]
pub mod admin;

#[cfg(feature = "config")]
pub mod config;

#[cfg(feature = "forms")]
pub mod forms;

/// DRF-style serializer layer — `#[derive(Serializer)]` + [`serializer::ModelSerializer`].
/// Typed JSON output from model instances with field control and validation.
#[cfg(feature = "serializer")]
pub mod serializer;

/// Pluggable caching layer — [`cache::Cache`] trait + [`cache::NullCache`] +
/// [`cache::InMemoryCache`]. Redis backend behind the `cache-redis` feature.
#[cfg(feature = "cache")]
pub mod cache;

/// Django-shape model signals — [`signals::connect_post_save`] etc.
/// Receivers register globally per model type and run sequentially.
#[cfg(feature = "signals")]
pub mod signals;

/// CORS middleware — [`cors::CorsLayer`] for axum routers.
#[cfg(feature = "admin")]
pub mod cors;

/// Token-bucket rate limiting middleware — [`rate_limit::RateLimitLayer`].
/// Per-IP, per-header, or global. Returns 429 with `Retry-After` when exhausted.
#[cfg(feature = "admin")]
pub mod rate_limit;

/// Health check endpoints — `/health` (liveness) + `/ready` (readiness).
/// See [`health::health_router`].
#[cfg(feature = "admin")]
pub mod health;

/// Email backends — [`email::Mailer`] trait + console/in-memory/null backends.
#[cfg(feature = "email")]
pub mod email;

#[cfg(feature = "tenancy")]
pub mod tenancy;

/// Per-request extractors for handlers — tenancy-aware DI. Today
/// ships [`extractors::Tenant`]; future slices add `Operator` + `User`.
#[cfg(feature = "tenancy")]
pub mod extractors;

/// DRF-style ModelViewSet — five REST endpoints for any [`Model`] table
/// in ~5 lines. See [`viewset::ViewSet`].
#[cfg(feature = "tenancy")]
pub mod viewset;

/// Django-style runserver — [`server::Builder`] owns every line of
/// boilerplate every tenancy app would otherwise rewrite (DB pool,
/// resolver chain, host dispatch, operator console, bind + serve).
#[cfg(feature = "tenancy")]
pub mod server;

/// `#[rustango::main]` — the Django-shape `runserver` entrypoint.
/// Wraps `#[tokio::main]` with a default `tracing-subscriber` boot
/// (env-filter, falling back to `info,sqlx=warn`). Available behind
/// the `runtime` feature, which `tenancy` implies.
#[cfg(feature = "runtime")]
pub use rustango_macros::main;

/// Internal re-exports for proc-macros that need to name third-party
/// crates without forcing the user to add them to their `Cargo.toml`.
/// Not part of the public API — names here may change between minors.
#[doc(hidden)]
#[cfg(feature = "runtime")]
pub mod __private_runtime {
    pub use tracing_subscriber;
}

/// Proc-macros crate, re-exported. End users normally reach
/// [`Model`] and [`embed_migrations`] directly via the facade rather
/// than naming `macros`.
pub use rustango_macros as macros;

/// `#[derive(Model)]` — populates the `inventory` registry the admin
/// walks, generates `objects()` / typed columns / `insert` / `delete`
/// / `save`.
pub use rustango_macros::Model;

/// Server-assigned PK wrapper. `id: Auto<i64>` → `BIGSERIAL`. See
/// [`sql::Auto`] for details.
pub use sql::Auto;

/// Bake every migration file in a directory into the binary at
/// compile time, for shipping a single-binary distribution. Pair
/// with [`migrate::migrate_embedded`].
pub use rustango_macros::embed_migrations;

/// `#[derive(Form)]` — implements [`forms::Form`] so a struct can be
/// parsed from an HTTP form payload with multi-error validation.
/// Re-exported only when the `forms` feature is on.
#[cfg(feature = "forms")]
pub use rustango_macros::Form;

/// `#[derive(ViewSet)]` — generates a `router(prefix, pool) -> axum::Router`
/// associated method on a marker struct, wiring the full CRUD ViewSet in one
/// annotation. Re-exported only when the `tenancy` feature is on.
#[cfg(feature = "tenancy")]
pub use rustango_macros::ViewSet;

/// `#[derive(Serializer)]` — implements [`serializer::ModelSerializer`] on a
/// struct, generating `from_model`, a custom `serde::Serialize` (respecting
/// `write_only`), and `writable_fields`. Re-exported when the `serializer`
/// feature is on.
#[cfg(feature = "serializer")]
pub use rustango_macros::Serializer;
