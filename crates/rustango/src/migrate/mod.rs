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
pub mod invert;
pub mod make;
pub mod manage;
mod runner;
pub mod scaffold;
pub mod snapshot;

pub use diff::{
    detect_changes, detect_unsupported_field_changes, render_changes, SchemaChange,
};
pub use error::MigrateError;
pub use file::{discover_migration_dirs, list_dirs, DataOp, Migration, MigrationScope, Operation};
pub use invert::invert;
pub use make::{make_migrations, make_migrations_for_app, make_migrations_from};
pub use runner::{
    applied_set, applied_set_pool, apply_all, apply_all_pool, downgrade, downgrade_pool,
    drop_all, drop_all_pool, ensure_ledger, ensure_ledger_pool, migrate, migrate_dry_run,
    migrate_dry_run_pool, migrate_embedded, migrate_pool, migrate_to, migrate_to_pool,
    registered_models, unapply, unapply_force, unapply_force_pool, unapply_pool, Builder,
    MigrationPreview, LEDGER_TABLE,
};
pub use snapshot::{FieldSnapshot, RelationSnapshot, SchemaSnapshot, TableSnapshot};
pub use manage::{append_data_op, make_data_migration};
