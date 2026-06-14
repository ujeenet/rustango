//! Named multi-database registry — Django's `DATABASES` setting +
//! `QuerySet.using(alias)` (issues #332 / #400).
//!
//! rustango is single-pool-per-call by default: every terminal takes an
//! explicit connection (`fetch(&pool)` / `fetch_on(executor)`), so
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
//! **Writes** still route through the explicit `fetch(&pool)` family
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

// ---- DATABASE_ROUTERS (#401) -----------------------------------------

/// A database router — Django's
/// [`DATABASE_ROUTERS`](https://docs.djangoproject.com/en/6.0/topics/db/multi-db/#database-routers)
/// (#401). Decides which registered alias a given model's reads / writes
/// should target, so callers get automatic read-replica / sharding
/// routing instead of threading an alias through every call.
///
/// Both methods default to `None` ("no opinion — defer to the next
/// router, then the `"default"` alias"), so an implementation overrides
/// only what it cares about. Routers are consulted in registration order;
/// the first `Some(alias)` wins.
///
/// ```ignore
/// struct ReadReplicaRouter;
/// impl rustango::databases::DatabaseRouter for ReadReplicaRouter {
///     // Send every read to the replica; writes fall through to "default".
///     fn db_for_read(&self, _model: &rustango::core::ModelSchema) -> Option<String> {
///         Some("replica".into())
///     }
/// }
/// rustango::databases::register_router(ReadReplicaRouter);
/// ```
pub trait DatabaseRouter: Send + Sync + 'static {
    /// Alias to read `model` from, or `None` to defer.
    fn db_for_read(&self, model: &crate::core::ModelSchema) -> Option<String> {
        let _ = model;
        None
    }
    /// Alias to write `model` to, or `None` to defer.
    fn db_for_write(&self, model: &crate::core::ModelSchema) -> Option<String> {
        let _ = model;
        None
    }
}

#[allow(clippy::type_complexity)]
static ROUTERS: OnceLock<RwLock<Vec<Box<dyn DatabaseRouter>>>> = OnceLock::new();

fn routers() -> &'static RwLock<Vec<Box<dyn DatabaseRouter>>> {
    ROUTERS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Append a router to the chain (Django's `DATABASE_ROUTERS` list).
/// Routers are consulted in registration order.
pub fn register_router(router: impl DatabaseRouter) {
    routers()
        .write()
        .expect("routers registry not poisoned")
        .push(Box::new(router));
}

/// Remove every registered router. Intended for test isolation.
pub fn clear_routers() {
    routers()
        .write()
        .expect("routers registry not poisoned")
        .clear();
}

/// The alias to **read** `model` from — the first router that returns
/// `Some`, or `None` if every router defers (caller falls back to
/// [`DEFAULT_ALIAS`]).
#[must_use]
pub fn route_read(model: &crate::core::ModelSchema) -> Option<String> {
    routers()
        .read()
        .expect("routers registry not poisoned")
        .iter()
        .find_map(|r| r.db_for_read(model))
}

/// The alias to **write** `model` to — see [`route_read`].
#[must_use]
pub fn route_write(model: &crate::core::ModelSchema) -> Option<String> {
    routers()
        .read()
        .expect("routers registry not poisoned")
        .iter()
        .find_map(|r| r.db_for_write(model))
}

/// Resolve the read pool for `model` via the router chain, falling back
/// to the `"default"` alias. Panics if the chosen alias isn't registered
/// (see [`pool`]).
#[must_use]
pub fn read_pool_for(model: &crate::core::ModelSchema) -> Pool {
    pool(&route_read(model).unwrap_or_else(|| DEFAULT_ALIAS.to_owned()))
}

/// Resolve the write pool for `model` via the router chain, falling back
/// to the `"default"` alias.
#[must_use]
pub fn write_pool_for(model: &crate::core::ModelSchema) -> Pool {
    pool(&route_write(model).unwrap_or_else(|| DEFAULT_ALIAS.to_owned()))
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
    /// explicit `fetch(&pool)` family for those.
    #[must_use]
    pub fn using(self, alias: &str) -> UsingQuerySet<T> {
        UsingQuerySet {
            qs: self,
            pool: pool(alias),
        }
    }

    /// Route this read through the registered [`DatabaseRouter`] chain —
    /// the automatic counterpart of [`Self::using`] (issue #401). The
    /// routers' `db_for_read(T::SCHEMA)` decision picks the alias (first
    /// `Some` wins), falling back to the `"default"` alias when every
    /// router defers. Returns a [`UsingQuerySet`] bound to that pool.
    ///
    /// Panics if the chosen alias isn't registered (see [`pool`]). Like
    /// [`Self::using`], this exposes only read terminals; for writes use
    /// [`write_pool_for`] (`fetch(&write_pool_for(T::SCHEMA))`) so a
    /// write is never silently sent to a read replica.
    #[must_use]
    pub fn routed(self) -> UsingQuerySet<T> {
        let pool = read_pool_for(T::SCHEMA);
        UsingQuerySet { qs: self, pool }
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
    /// `fetch(&pool)` but routed by alias.
    ///
    /// # Errors
    /// As [`crate::sql::FetcherPool::fetch`].
    pub async fn fetch(self) -> Result<Vec<T>, crate::sql::ExecError> {
        use crate::sql::FetcherPool as _;
        self.qs.fetch(&self.pool).await
    }

    /// The first matching row (applies `LIMIT 1`).
    ///
    /// # Errors
    /// As [`Self::fetch`].
    pub async fn first(self) -> Result<Option<T>, crate::sql::ExecError> {
        use crate::sql::FetcherPool as _;
        Ok(self.qs.limit(1).fetch(&self.pool).await?.into_iter().next())
    }

    /// `SELECT COUNT(*)` against the chosen connection.
    ///
    /// # Errors
    /// As [`crate::sql::CounterPool::count`].
    pub async fn count(self) -> Result<i64, crate::sql::ExecError> {
        use crate::sql::CounterPool as _;
        self.qs.count(&self.pool).await
    }

    /// `EXISTS` against the chosen connection.
    ///
    /// # Errors
    /// As [`crate::sql::ExistsPool::exists`].
    pub async fn exists(self) -> Result<bool, crate::sql::ExecError> {
        use crate::sql::ExistsPool as _;
        self.qs.exists(&self.pool).await
    }
}
