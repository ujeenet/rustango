//! Migrations for rustango.
//!
//! A migration is an on-disk file ([`file::Migration`]) holding a schema
//! snapshot plus an ordered list of schema and data operations. The
//! runner applies pending files and records each one in a
//! `__rustango_migrations__` ledger; rollback replays the inverse.
//!
//! `make_migrations` writes new files by diffing the model registry
//! against the last snapshot. `migrate` and `downgrade` apply and undo
//! them.

pub mod callbacks;
pub mod ddl;
pub mod diff;
/// Applying rendered DDL idempotently, for the `ensure_*_table` helpers.
pub mod ensure;
mod error;
pub mod file;
// The CLI's "emit Model derives from a live schema" verb. Works on any
// backend: `information_schema` on PG and MySQL, PRAGMA plus
// `sqlite_master` on SQLite.
pub(crate) mod inspectdb;
pub mod invert;
pub mod make;
// The migrate CLI dispatcher. `manage::run` takes a `&crate::sql::Pool`
// and routes each verb to its companion in `crate::migrate::runner`.
pub mod manage;
/// Watching a migration run while it happens: the observer the
/// progress-reporting entry points take.
pub mod progress;
mod runner;
pub mod scaffold;
pub mod snapshot;
/// Rewriting old `SQLite` datetime columns onto the one text shape that
/// compares correctly against a Rust-bound timestamp.
#[cfg(feature = "sqlite")]
pub mod sqlite_datetime;

pub use diff::{
    detect_changes, detect_unsupported_field_changes, render_changes,
    render_changes_split_with_dialect, RenderedBatch, SchemaChange,
};
pub use ensure::apply_idempotent;
pub use error::MigrateError;
pub use file::{
    discover_migration_dirs, list_dirs, CallbackOp, DataOp, Migration, MigrationScope, Operation,
};
pub use invert::invert;
pub use make::{
    make_migrations, make_migrations_for_app, make_migrations_from, make_migrations_system,
};
#[cfg(feature = "postgres")]
pub use manage::{append_data_op, make_data_migration};
pub use progress::{MigrationEvent, MigrationObserver, Outcome};
pub use runner::ensure_ledger_pool_with_ledger;
pub use runner::migrate_pool_with_ledger;
pub use runner::migrate_pool_with_ledger_fake_initial;
pub use runner::migrate_pool_with_ledger_fake_initial_with_progress;
pub use runner::migrate_pool_with_progress;
// Always on: entry points that work on PG, MySQL and SQLite through the
// `Pool` enum, plus the inventory and builder surface.
pub use runner::{
    applied_set_pool, apply_all_pool, downgrade_pool, drop_all_pool, ensure_ledger_pool,
    migrate_dry_run_pool, migrate_embedded_pool, migrate_pool, migrate_to_pool, registered_models,
    sqlmigrate_one, unapply_force_pool, unapply_pool, Builder, MigrationPreview, LEDGER_TABLE,
};
// PG-typed back-compat, only when the `postgres` feature is on.
// SQLite and MySQL apps use the `_pool` variants above.
#[cfg(feature = "postgres")]
pub use runner::{
    applied_set, apply_all, downgrade, drop_all, ensure_ledger, migrate, migrate_dry_run,
    migrate_embedded, migrate_to, migrate_with_progress, unapply, unapply_force,
};
pub use snapshot::{FieldSnapshot, IndexSnapshot, RelationSnapshot, SchemaSnapshot, TableSnapshot};
