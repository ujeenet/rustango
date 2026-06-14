//! Model pruning — Eloquent `Prunable` / Django "scheduled bulk
//! removal" parity. Issue #822.
//!
//! ## Why
//!
//! Long-lived tables accumulate stale rows: expired sessions,
//! tombstoned soft-deletes past their retention window, archived
//! audit entries, old job-queue records. Django solves this with
//! ad-hoc management commands; Eloquent has a declarative
//! `Prunable` / `MassPrunable` trait + `model:prune` CLI. Rustango's
//! shape sits between the two: declare a `Prunable` impl per model
//! (returning the queryset of rows TO DELETE), wire it via
//! [`register_prunable!`], and either invoke `manage prune` from the
//! CLI or call [`prune_all`] from app code (cron, background job,
//! startup hook).
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

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use crate::core::Model;
use crate::query::QuerySet;
use crate::sql::{delete_pool, CounterPool, ExecError, Pool};

/// Boxed future returned by an inventory-registered prune /count
/// thunk. `Send + 'a` so the inventory-iteration loop can await
/// each entry sequentially without lifetime gymnastics.
type ResultFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ExecError>> + Send + 'a>>;

/// Trait declaring "this model is prunable; here's the queryset of
/// rows that should be deleted on the next prune run." Implementors
/// must also call [`register_prunable!`] to advertise the impl to
/// the inventory walker (and the `manage prune` CLI verb).
///
/// Returning an empty queryset is fine — a prune run over a model
/// with no stale rows is a no-op.
///
/// The queryset's filters / ordering / limits are honored verbatim.
/// The framework appends nothing on top of what `prune_queryset`
/// returns, so safety-belt filters (`deleted_at IS NOT NULL`,
/// `created < cutoff`) MUST be set inside the impl.
pub trait Prunable: Model {
    /// The set of rows to delete on the next prune run. Called once
    /// per CLI invocation (or once per `prune_all`).
    fn prune_queryset() -> QuerySet<Self>;
}

/// One inventory-collected prunable registration. End users don't
/// construct these directly — go through [`register_prunable!`].
///
/// `name` defaults to the model's `SCHEMA.table` so CLI args
/// (`--model <name>` / `--except <name>`) match the table name.
#[doc(hidden)]
pub struct PrunableEntry {
    /// Canonical name — match against CLI `--model` / `--except`.
    /// Defaults to `<T as Model>::SCHEMA.table`.
    pub name: &'static str,
    /// Count fn used by `--pretend`. Returns the row count the
    /// `prune` fn would delete.
    pub count: fn(&Pool) -> ResultFuture<'_, i64>,
    /// Prune fn — actually executes the DELETE. Returns rows
    /// affected.
    pub prune: fn(&Pool) -> ResultFuture<'_, u64>,
}

inventory::collect!(PrunableEntry);

/// Internal helper invoked from [`register_prunable!`] to build the
/// per-model count thunk. Public-but-hidden because it's referenced
/// from the macro's expanded body.
#[doc(hidden)]
pub fn __count_thunk<T>(pool: &Pool) -> ResultFuture<'_, i64>
where
    T: Prunable + Send + 'static,
    QuerySet<T>: CounterPool<T>,
{
    Box::pin(async move { T::prune_queryset().count(pool).await })
}

/// Internal helper invoked from [`register_prunable!`] to build the
/// per-model prune thunk.
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

/// Register a [`Prunable`] impl with the inventory walker. The
/// `manage prune` CLI verb walks every entry on each run; the
/// in-process [`prune_all`] / [`prune_pretend`] helpers do the same.
///
/// ```ignore
/// rustango::register_prunable!(AuditEntry);
/// ```
///
/// The macro takes the model type only — the registration name is
/// inferred from `<T as Model>::SCHEMA.table`.
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

/// Selection knobs for [`prune_all`] / [`prune_pretend`] / the CLI.
#[derive(Debug, Clone, Default)]
pub struct PruneOptions {
    /// When non-empty, only models in this list run. Names match
    /// `PrunableEntry::name` (i.e. the model's SQL table name).
    /// Empty (the default) means "every registered prunable."
    pub only: Vec<String>,
    /// Models in this list are skipped even when they would
    /// otherwise run. Takes precedence over `only`.
    pub except: Vec<String>,
}

impl PruneOptions {
    /// `true` when this entry should run under the current options.
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

/// One row in the report returned by [`prune_all`] / [`prune_pretend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// Model's SQL table name.
    pub table: String,
    /// Rows actually deleted (for `prune_all`) or rows the prune
    /// WOULD delete (for `prune_pretend`).
    pub rows: u64,
}

/// Walk every registered prunable, deleting matching rows. Honors
/// [`PruneOptions::only`] / `except`. Returns one [`PruneReport`]
/// per model that actually ran (skipped models produce no entry).
///
/// Use [`prune_pretend`] for the dry-run variant.
///
/// # Errors
///
/// Each entry's prune is awaited sequentially. The FIRST error
/// short-circuits the loop and propagates — every entry up to the
/// failure has already executed (and its rows are deleted); the
/// report Vec contains the successful entries.
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

/// `--pretend` companion to [`prune_all`]. Walks the same registry
/// but invokes each entry's `count` thunk instead of `prune` — no
/// rows are deleted. Useful for previewing the impact of a prune
/// against production data before committing.
///
/// # Errors
///
/// Same shape as [`prune_all`] — first error short-circuits.
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
            // `count` returns i64; clamp the negative-impossible
            // case to 0 so the reporting type stays positive.
            rows: u64::try_from(rows.max(0)).unwrap_or(0),
        });
    }
    Ok(reports)
}

/// Names of every model currently registered as prunable. Useful
/// for diagnostic CLI output ("known prunable tables: …") + for
/// validating `--model X` / `--except X` flags at parse time.
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
