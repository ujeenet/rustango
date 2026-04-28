//! Migrations for rustango.
//!
//! v0.1 shipped a Postgres DDL writer plus an `apply_all` runner that
//! walks the inventory registry and emits `CREATE TABLE` per
//! `#[derive(Model)]`. Good for bootstrap, no good for evolving schema.
//!
//! v0.2 adds **schema snapshots**: capture the registry as JSON, diff
//! against a previous snapshot to produce `CREATE TABLE` / `DROP TABLE`
//! / `ADD COLUMN` / `DROP COLUMN` DDL, and persist the file as the next
//! migration.

pub mod ddl;
pub mod diff;
mod error;
mod runner;
pub mod snapshot;

pub use diff::{detect_changes, render_changes, SchemaChange};
pub use error::MigrateError;
pub use runner::{apply_all, drop_all, registered_models};
pub use snapshot::{FieldSnapshot, RelationSnapshot, SchemaSnapshot, TableSnapshot};
