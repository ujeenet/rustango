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

pub mod core;
pub mod migrate;
pub mod query;
pub mod sql;

#[cfg(feature = "admin")]
pub mod admin;

#[cfg(feature = "tenancy")]
pub mod tenancy;

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
