//! rustango — a Django-inspired ORM for Rust.
//!
//! v0.1 status: scaffolding only. See the workspace plan for milestones.

pub use rustango_admin as admin;
pub use rustango_core as core;
pub use rustango_macros as macros;
pub use rustango_migrate as migrate;
pub use rustango_query as query;
pub use rustango_sql as sql;

/// `#[derive(Model)]` — see [`macros`] for the supported attributes.
pub use rustango_macros::Model;

/// Server-assigned PK wrapper. `id: Auto<i64>` → `BIGSERIAL`. See
/// [`sql::Auto`] for details.
pub use rustango_sql::Auto;

/// Bake every migration file in a directory into the binary at
/// compile time, for shipping a single-binary distribution. Pair
/// with [`migrate::migrate_embedded`].
pub use rustango_macros::embed_migrations;
