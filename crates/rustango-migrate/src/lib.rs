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
pub mod make;
mod runner;
pub mod snapshot;

pub use diff::{detect_changes, render_changes, SchemaChange};
pub use error::MigrateError;
pub use file::{DataOp, Migration, Operation};
pub use make::{make_migrations, make_migrations_from};
pub use runner::{apply_all, drop_all, registered_models};
pub use snapshot::{FieldSnapshot, RelationSnapshot, SchemaSnapshot, TableSnapshot};
