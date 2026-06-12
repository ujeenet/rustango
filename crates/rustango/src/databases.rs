//! Named multi-database registry — Django's `DATABASES` setting +
//! `QuerySet.using(alias)` (issues #332 / #400).
//!
//! rustango is single-pool-per-call by default: every terminal takes an
//! explicit connection (`fetch_pool(&pool)` / `fetch_on(executor)`), so
//! multi-DB routing is already possible by passing the right pool. This
//! module adds the Django-shaped **named-alias** convenience on top: a
//! process-wide registry of `alias → Pool` plus a `.using("alias")` verb
//! that resolves the alias and runs against the matching pool — the
//! read-replica / multi-DB ergonomics without threading a `Pool` through
//! every call site.
//!
//! ```ignore
//! // At startup (the `DATABASES` equivalent):
//! rustango::databases::register("default", primary_pool);
//! rustango::databases::register("replica", replica_pool);
//!
//! // Route a read to the replica — Django's `.using("replica")`:
//! let posts = Post::objects()
//!     .filter("published", true)
//!     .using("replica")
//!     .fetch()
//!     .await?;
//! ```
//!
//! **Writes** still route through the explicit `fetch_pool(&pool)` family
//! on purpose — `.using` exposes only the read terminals so a write
//! can't be silently sent to a read replica. Automatic per-model routing
//! (Django's `DATABASE_ROUTERS`, #401) is a separate layer on top of this
//! registry.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::sql::Pool;

/// The conventional alias for the primary connection (Django's
/// `DATABASES["default"]`).
pub const DEFAULT_ALIAS: &str = "default";

static REGISTRY: OnceLock<RwLock<HashMap<String, Pool>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, Pool>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register (or replace) the connection pool under `alias`. Call once
/// per database at startup. The `"default"` alias is the conventional
/// primary; any other name (`"replica"`, `"analytics"`, …) is yours.
pub fn register(alias: impl Into<String>, pool: impl Into<Pool>) {
    registry()
        .write()
        .expect("databases registry not poisoned")
        .insert(alias.into(), pool.into());
}

/// Resolve `alias` to its pool, or `None` if nothing is registered under
/// it. Prefer [`pool`] when the alias is expected to exist.
#[must_use]
pub fn get(alias: &str) -> Option<Pool> {
    registry()
        .read()
        .expect("databases registry not poisoned")
        .get(alias)
        .cloned()
}

/// The `"default"` connection, if registered.
#[must_use]
pub fn default() -> Option<Pool> {
    get(DEFAULT_ALIAS)
}

/// Resolve `alias`, panicking with a clear message if it isn't
/// registered — the rustango analogue of Django's
/// `ConnectionDoesNotExist`. An unknown alias is a startup-wiring bug,
/// so this fails loudly rather than silently picking a wrong database.
#[must_use]
pub fn pool(alias: &str) -> Pool {
    get(alias).unwrap_or_else(|| {
        panic!(
            "no database registered under alias `{alias}` — \
             call `rustango::databases::register(\"{alias}\", pool)` at startup \
             (registered: {:?})",
            aliases()
        )
    })
}

/// Every registered alias, sorted — handy for diagnostics / `manage`
/// introspection.
#[must_use]
pub fn aliases() -> Vec<String> {
    let mut v: Vec<String> = registry()
        .read()
        .expect("databases registry not poisoned")
        .keys()
        .cloned()
        .collect();
    v.sort();
    v
}

/// Remove every registered connection. Intended for test isolation.
pub fn clear() {
    registry()
        .write()
        .expect("databases registry not poisoned")
        .clear();
}

impl<T: crate::core::Model> crate::query::QuerySet<T> {
    /// Route this queryset to the connection registered under `alias` —
    /// Django's [`QuerySet.using(alias)`](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#using).
    /// Issue #332.
    ///
    /// Returns a [`UsingQuerySet`] exposing the read terminals
    /// (`fetch` / `first` / `count` / `exists`) bound to the resolved
    /// pool. Panics at call time if `alias` isn't registered (see
    /// [`pool`]). Writes intentionally aren't routed here — use the
    /// explicit `fetch_pool(&pool)` family for those.
    #[must_use]
    pub fn using(self, alias: &str) -> UsingQuerySet<T> {
        UsingQuerySet {
            qs: self,
            pool: pool(alias),
        }
    }
}

/// A queryset bound to a specific registered connection via
/// [`QuerySet::using`]. Carries the read terminals that resolve against
/// the chosen pool.
pub struct UsingQuerySet<T: crate::core::Model> {
    qs: crate::query::QuerySet<T>,
    pool: Pool,
}

impl<T> UsingQuerySet<T>
where
    T: crate::core::Model
        + crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Run the query against the chosen connection — like
    /// `fetch_pool(&pool)` but routed by alias.
    ///
    /// # Errors
    /// As [`crate::sql::FetcherPool::fetch_pool`].
    pub async fn fetch(self) -> Result<Vec<T>, crate::sql::ExecError> {
        use crate::sql::FetcherPool as _;
        self.qs.fetch_pool(&self.pool).await
    }

    /// The first matching row (applies `LIMIT 1`).
    ///
    /// # Errors
    /// As [`Self::fetch`].
    pub async fn first(self) -> Result<Option<T>, crate::sql::ExecError> {
        use crate::sql::FetcherPool as _;
        Ok(self
            .qs
            .limit(1)
            .fetch_pool(&self.pool)
            .await?
            .into_iter()
            .next())
    }

    /// `SELECT COUNT(*)` against the chosen connection.
    ///
    /// # Errors
    /// As [`crate::sql::CounterPool::count_pool`].
    pub async fn count(self) -> Result<i64, crate::sql::ExecError> {
        use crate::sql::CounterPool as _;
        self.qs.count_pool(&self.pool).await
    }

    /// `EXISTS` against the chosen connection.
    ///
    /// # Errors
    /// As [`crate::sql::ExistsPool::exists_pool`].
    pub async fn exists(self) -> Result<bool, crate::sql::ExecError> {
        use crate::sql::ExistsPool as _;
        self.qs.exists_pool(&self.pool).await
    }
}
