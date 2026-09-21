//! Model pruning: delete stale rows on a schedule.
//!
//! Long-lived tables fill up with expired sessions, old soft-deletes,
//! aged audit entries and finished jobs. Write a [`Prunable`] impl
//! that returns the queryset of rows to delete, register it with
//! [`register_prunable!`], then run `manage prune` or call
//! [`prune_all`] from cron or a background job.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::prunable::Prunable;
//! use rustango::query::QuerySet;
//! use rustango::Model;
//!
//! #[derive(Model)]
//! struct AuditEntry {
//!     #[rustango(primary_key)] id: rustango::Auto<i64>,
//!     created: chrono::DateTime<chrono::Utc>,
//!     // ...
//! }
//!
//! impl Prunable for AuditEntry {
//!     fn prune_queryset() -> QuerySet<Self> {
//!         // Anything older than 30 days is fair game.
//!         let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
//!         QuerySet::<Self>::default().filter("created__lt", cutoff)
//!     }
//! }
//!
//! rustango::register_prunable!(AuditEntry);
//! ```
//!
//! Then run from the CLI:
//!
//! ```text
//! cargo run -- prune                  # delete all registered models
//! cargo run -- prune --model AuditEntry
//! cargo run -- prune --except Sessions
//! cargo run -- prune --pretend        # count matches without deleting
//! ```
//!
//! Or programmatically from a background job:
//!
//! ```ignore
//! let reports = rustango::prunable::prune_all(
//!     &pool,
//!     &rustango::prunable::PruneOptions::default(),
//! ).await?;
//! for r in reports {
//!     tracing::info!(table = %r.table, rows = r.rows, "pruned");
//! }
//! ```
//!
//! [`Prunable`]: crate::prunable::Prunable
//! [`prune_all`]: crate::prunable::prune_all
//! [`register_prunable!`]: crate::register_prunable

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use crate::core::Model;
use crate::query::QuerySet;
use crate::sql::{delete_pool, CounterPool, ExecError, Pool};

/// Boxed future returned by a registered prune or count thunk.
type ResultFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ExecError>> + Send + 'a>>;

/// Marks a model as prunable and says which rows to delete. Also call
/// [`register_prunable!`](crate::register_prunable) so `manage prune`
/// can find the impl.
///
/// An empty queryset is fine; the run just deletes nothing.
///
/// The queryset is used exactly as you return it. The framework adds
/// no filter of its own, so your cutoff conditions, such as
/// `created < cutoff` or `deleted_at IS NOT NULL`, must be in the
/// impl. A queryset with no filter deletes the whole table.
pub trait Prunable: Model {
    /// The rows to delete. Called once per prune run.
    fn prune_queryset() -> QuerySet<Self>;
}

/// One registration. Build it with [`register_prunable!`].
#[doc(hidden)]
pub struct PrunableEntry {
    /// The model's table name, matched by `--model` and `--except`.
    pub name: &'static str,
    /// Counts the rows `prune` would delete. Used by `--pretend`.
    pub count: fn(&Pool) -> ResultFuture<'_, i64>,
    /// Runs the DELETE and returns rows affected.
    pub prune: fn(&Pool) -> ResultFuture<'_, u64>,
}

inventory::collect!(PrunableEntry);

/// Count thunk for [`register_prunable!`]. Public only because the
/// macro expands to a reference to it.
#[doc(hidden)]
pub fn __count_thunk<T>(pool: &Pool) -> ResultFuture<'_, i64>
where
    T: Prunable + Send + 'static,
    QuerySet<T>: CounterPool<T>,
{
    Box::pin(async move { T::prune_queryset().count(pool).await })
}

/// Prune thunk for [`register_prunable!`].
#[doc(hidden)]
pub fn __prune_thunk<T>(pool: &Pool) -> ResultFuture<'_, u64>
where
    T: Prunable + Send + 'static,
{
    Box::pin(async move {
        let query = T::prune_queryset().compile_delete()?;
        delete_pool(pool, &query).await
    })
}

/// Register a [`Prunable`] impl so `manage prune`, [`prune_all`] and
/// [`prune_pretend`] can find it. Pass the model type only; the name
/// comes from its table.
///
/// ```ignore
/// rustango::register_prunable!(AuditEntry);
/// ```
#[macro_export]
macro_rules! register_prunable {
    ($t:ty) => {
        $crate::inventory::submit! {
            $crate::prunable::PrunableEntry {
                name: <$t as $crate::core::Model>::SCHEMA.table,
                count: $crate::prunable::__count_thunk::<$t>,
                prune: $crate::prunable::__prune_thunk::<$t>,
            }
        }
    };
}

/// Picks which models a prune run covers.
#[derive(Debug, Clone, Default)]
pub struct PruneOptions {
    /// Run only these table names. Empty means all of them.
    pub only: Vec<String>,
    /// Always skip these table names. Wins over `only`.
    pub except: Vec<String>,
}

impl PruneOptions {
    /// `true` when this entry should run.
    fn allows(&self, name: &str) -> bool {
        if self.except.iter().any(|s| s == name) {
            return false;
        }
        if self.only.is_empty() {
            return true;
        }
        self.only.iter().any(|s| s == name)
    }
}

/// One line of a prune report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// The model's table name.
    pub table: String,
    /// Rows deleted, or rows that would be deleted under
    /// [`prune_pretend`].
    pub rows: u64,
}

/// Delete matching rows for every registered prunable that
/// [`PruneOptions`] allows. Returns one [`PruneReport`] per model that
/// ran. Use [`prune_pretend`] for a dry run.
///
/// Under tenancy, pass a tenant-scoped pool. This prunes only the
/// tenant that `pool` points at, and in schema mode a registry pool
/// reaches only `public`. Loop over tenants with
/// [`crate::tenancy::for_each_tenant`] to cover them all.
///
/// # Errors
///
/// Entries run one after another and the first error stops the loop.
/// Rows deleted before that point stay deleted, and the returned
/// report covers only the entries that finished.
pub async fn prune_all(pool: &Pool, opts: &PruneOptions) -> Result<Vec<PruneReport>, ExecError> {
    let mut reports = Vec::new();
    for entry in inventory::iter::<PrunableEntry> {
        if !opts.allows(entry.name) {
            continue;
        }
        let rows = (entry.prune)(pool).await?;
        reports.push(PruneReport {
            table: entry.name.to_owned(),
            rows,
        });
    }
    Ok(reports)
}

/// Dry run of [`prune_all`]: counts the rows instead of deleting
/// them. Use it to preview a prune before you run it for real.
///
/// # Errors
///
/// As [`prune_all`]: the first error stops the loop.
pub async fn prune_pretend(
    pool: &Pool,
    opts: &PruneOptions,
) -> Result<Vec<PruneReport>, ExecError> {
    let mut reports = Vec::new();
    for entry in inventory::iter::<PrunableEntry> {
        if !opts.allows(entry.name) {
            continue;
        }
        let rows = (entry.count)(pool).await?;
        reports.push(PruneReport {
            table: entry.name.to_owned(),
            // `count` is i64; clamp so the report stays unsigned.
            rows: u64::try_from(rows.max(0)).unwrap_or(0),
        });
    }
    Ok(reports)
}

/// Every registered prunable table name. Use it to list them, or to
/// check a `--model` or `--except` flag before running.
#[must_use]
pub fn registered_names() -> Vec<&'static str> {
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut out = Vec::new();
    for entry in inventory::iter::<PrunableEntry> {
        if seen.insert(entry.name) {
            out.push(entry.name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_allow_when_empty() {
        let opts = PruneOptions::default();
        assert!(opts.allows("foo"));
        assert!(opts.allows("bar"));
    }

    #[test]
    fn options_only_restricts_to_listed() {
        let opts = PruneOptions {
            only: vec!["foo".into()],
            ..PruneOptions::default()
        };
        assert!(opts.allows("foo"));
        assert!(!opts.allows("bar"));
    }

    #[test]
    fn options_except_excludes_listed() {
        let opts = PruneOptions {
            except: vec!["foo".into()],
            ..PruneOptions::default()
        };
        assert!(!opts.allows("foo"));
        assert!(opts.allows("bar"));
    }

    #[test]
    fn options_except_takes_precedence_over_only() {
        let opts = PruneOptions {
            only: vec!["foo".into(), "bar".into()],
            except: vec!["foo".into()],
        };
        assert!(!opts.allows("foo"));
        assert!(opts.allows("bar"));
        assert!(!opts.allows("baz"));
    }
}
