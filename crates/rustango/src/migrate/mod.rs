//! Migrations for rustango.
//!
//! v0.1 shipped a Postgres DDL writer plus an `apply_all` runner that
//! walks the inventory registry and emits `CREATE TABLE` per
//! `#[derive(Model)]`. Good for bootstrap, no good for evolving schema.
//!
//! v0.2 added **schema snapshots**: capture the registry as JSON, diff
//! against a previous snapshot to produce `CREATE TABLE` / `DROP TABLE`
//! / `ADD COLUMN` / `DROP COLUMN` DDL.
//!
//! v0.3 adds **on-disk migration files** ([`file::Migration`]) that
//! carry both the snapshot and a flat ordered list of schema-or-data
//! operations, plus an apply/rollback runner backed by a
//! `__rustango_migrations__` ledger. The `make_migrations` /
//! `migrate` / `downgrade` UX wraps it all (Slices 2-6).

pub mod ddl;
pub mod diff;
mod error;
pub mod file;
// inspectdb is the migrate CLI's "emit Model derives from a live PG
// schema" verb — PG-only by design (reads `information_schema` with
// PG-typed FromRow / pg_catalog joins). Gated to PG; sqlite/MySQL
// apps don't get this verb.
#[cfg(feature = "postgres")]
pub(crate) mod inspectdb;
pub mod invert;
pub mod make;
// v0.35 — the migrate CLI dispatcher (`migrate::manage::run`) is
// PG-typed end-to-end (takes `&PgPool`, threads it into subcommands
// that compile dialect-specific SQL via the PG-only runner paths).
// Gate the whole module on `postgres` for now; sqlite/MySQL apps
// build their own dispatcher around `migrate_pool` directly.
#[cfg(feature = "postgres")]
pub mod manage;
mod runner;
pub mod scaffold;
pub mod snapshot;

pub use diff::{detect_changes, detect_unsupported_field_changes, render_changes, SchemaChange};
pub use error::MigrateError;
pub use file::{discover_migration_dirs, list_dirs, DataOp, Migration, MigrationScope, Operation};
pub use invert::invert;
pub use make::{make_migrations, make_migrations_for_app, make_migrations_from};
#[cfg(feature = "postgres")]
pub use manage::{append_data_op, make_data_migration};
// Always-on: tri-dialect entry points (work on PG / MySQL / SQLite via
// the `Pool` enum), plus the inventory + builder surface.
pub use runner::{
    applied_set_pool, apply_all_pool, downgrade_pool, drop_all_pool, ensure_ledger_pool,
    migrate_dry_run_pool, migrate_embedded_pool, migrate_pool, migrate_to_pool, registered_models,
    unapply_force_pool, unapply_pool, Builder, MigrationPreview, LEDGER_TABLE,
};
// PG-typed back-compat: only re-exported when the `postgres` feature
// is on. Sqlite/MySQL apps use the `_pool` variants above.
#[cfg(feature = "postgres")]
pub use runner::{
    applied_set, apply_all, downgrade, drop_all, ensure_ledger, migrate, migrate_dry_run,
    migrate_embedded, migrate_to, unapply, unapply_force,
};
pub use snapshot::{FieldSnapshot, IndexSnapshot, RelationSnapshot, SchemaSnapshot, TableSnapshot};
