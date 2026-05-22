//! Django-shape `RunPython` — named Rust callbacks invoked during a
//! migration's apply / unapply walk. Issue #347.
//!
//! Where Django's `RunPython(forward_func, reverse_func)` takes Python
//! function references inside the migration file, rustango migration
//! files are JSON — they can't carry function pointers. Instead, the
//! callback is registered at startup via [`register_migration_callback!`]
//! and referenced by name in the JSON:
//!
//! ```json
//! {
//!   "name": "0003_backfill_user_locale",
//!   "forward": [
//!     {"callback": {"name": "backfill_locale"}},
//!     {"schema": ...}
//!   ]
//! }
//! ```
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
//! Names are inventory-collected; the lookup is `O(N)` over the global
//! registry but `N` is small (bounded by the number of declared
//! callbacks across the binary). Unknown names surface
//! [`MigrateError::Validation`] at apply time.

use std::future::Future;
use std::pin::Pin;

use crate::migrate::MigrateError;
use crate::sql::Pool;

/// Future returned by a migration callback. `'static` because the
/// callback is stored as a `fn` pointer and may run after the caller
/// has returned.
pub type MigrationCallbackFut =
    Pin<Box<dyn Future<Output = Result<(), MigrateError>> + Send + 'static>>;

/// Function signature a migration callback implements. Takes an owned
/// `Pool` (cheap — internally `Arc<...>`) so the future can outlive
/// the surrounding stack frame.
pub type MigrationCallbackFn = fn(Pool) -> MigrationCallbackFut;

/// One callback registration. Inventory-collected; submit via the
/// [`register_migration_callback!`] macro.
pub struct MigrationCallback {
    /// Name referenced from migration JSON `{"callback": {"name": "..."}}`.
    pub name: &'static str,
    /// The function pointer invoked on the migration's forward apply.
    /// Failures are surfaced as [`MigrateError::Validation`] / driver
    /// errors per the helper used inside the callback.
    pub forward: MigrationCallbackFn,
}

inventory::collect!(MigrationCallback);

/// Look up a registered callback by name. Returns `None` when no
/// callback with that name was submitted to the inventory — the runner
/// surfaces this as a validation error at apply time.
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
        // No `register_migration_callback!` in this test binary →
        // unknown lookups return None.
        assert!(find("nonexistent").is_none());
    }
}
