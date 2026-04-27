//! Migrations for rustango.
//!
//! v0.1 ships a Postgres DDL writer and an `apply_all` runner that walks
//! the inventory registry and emits `CREATE TABLE` for every
//! `#[derive(Model)]` in the binary. Snapshot/diff/state-tracking land
//! in v0.2 — for now this is the simplest useful thing: bootstrap a
//! schema directly from the model code.

pub mod ddl;
mod error;
mod runner;

pub use error::MigrateError;
pub use runner::{apply_all, drop_all, registered_models};
