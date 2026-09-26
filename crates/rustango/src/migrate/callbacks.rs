//! Named Rust callbacks run during a migration's apply or unapply
//! walk, for data changes that DDL alone cannot express.
//!
//! Migration files are JSON, so they cannot hold function pointers.
//! Register the callback at startup with
//! [`register_migration_callback!`] and refer to it by name:
//!
//! ```json
//! {
//!   "name": "0003_backfill_user_locale",
//!   "atomic": false,
//!   "forward": [
//!     {"schema": ...},
//!     {"callback": {"name": "backfill_locale"}}
//!   ]
//! }
//! ```
//!
//! Two things in that file are load-bearing.
//!
//! The schema op comes **first**: the callback backfills the column,
//! so it cannot run before the column exists. The example used to have
//! these the other way round.
//!
//! `"atomic": false` is **required** whenever a migration has a
//! callback, and the loader refuses the file without it. A callback is
//! handed a `Pool`, not the migration's open transaction, so it works
//! on a second connection — and inside the transaction that connection
//! waits on locks the transaction is holding. On PostgreSQL that hangs
//! forever rather than failing, because the first connection is `idle
//! in transaction` and the deadlock detector sees no cycle (#1626).
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::migrate::callbacks::{register_migration_callback, MigrationCallbackFut};
//! use rustango::sql::Pool;
//! use std::pin::Pin;
//!
//! fn backfill_locale(pool: Pool) -> MigrationCallbackFut {
//!     Box::pin(async move {
//!         rustango::sql::raw_execute_pool(
//!             &pool,
//!             r#"UPDATE "user" SET "locale" = 'en' WHERE "locale" IS NULL"#,
//!             Vec::new(),
//!         ).await.map_err(|e| rustango::migrate::MigrateError::Validation(e.to_string()))
//!     })
//! }
//!
//! rustango::register_migration_callback!("backfill_locale", backfill_locale);
//! ```
//!
//! Names are collected by `inventory`. Lookup scans the registry, but
//! it only holds the callbacks declared in the binary. An unknown name
//! raises [`MigrateError::Validation`] at apply time.
//!
//! [`register_migration_callback!`]: crate::register_migration_callback

use std::future::Future;
use std::pin::Pin;

use crate::migrate::MigrateError;
use crate::sql::Pool;

/// Future returned by a migration callback. `'static` because the
/// callback may run after the caller has returned.
pub type MigrationCallbackFut =
    Pin<Box<dyn Future<Output = Result<(), MigrateError>> + Send + 'static>>;

/// Signature a migration callback implements. The `Pool` is owned but
/// cheap to clone, so the future can outlive the calling frame.
pub type MigrationCallbackFn = fn(Pool) -> MigrationCallbackFut;

/// One callback registration. Submit it with the
/// [`register_migration_callback!`](crate::register_migration_callback)
/// macro.
pub struct MigrationCallback {
    /// Name used in migration JSON: `{"callback": {"name": "..."}}`.
    pub name: &'static str,
    /// The function the runner calls when it reaches this operation.
    pub forward: MigrationCallbackFn,
}

inventory::collect!(MigrationCallback);

/// Look up a registered callback by name. `None` means nothing
/// registered under that name; the runner turns that into a validation
/// error at apply time.
#[must_use]
pub fn find(name: &str) -> Option<&'static MigrationCallback> {
    inventory::iter::<MigrationCallback>
        .into_iter()
        .find(|c| c.name == name)
}

/// Register a named migration callback. Pair the chosen name with a
/// `{"callback": {"name": "..."}}` entry in your migration JSON's
/// `forward` array.
///
/// ```ignore
/// fn my_backfill(pool: rustango::sql::Pool) -> rustango::migrate::callbacks::MigrationCallbackFut {
///     Box::pin(async move {
///         // do the work
///         Ok(())
///     })
/// }
/// rustango::register_migration_callback!("my_backfill", my_backfill);
/// ```
#[macro_export]
macro_rules! register_migration_callback {
    ($name:expr, $forward:expr) => {
        $crate::inventory::submit! {
            $crate::migrate::callbacks::MigrationCallback {
                name: $name,
                forward: $forward,
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_compiles_with_zero_entries() {
        // Nothing registered in this test binary, so lookups miss.
        assert!(find("nonexistent").is_none());
    }
}
