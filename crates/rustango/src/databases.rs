//! A registry of named databases, plus `QuerySet::using(alias)` to
//! route a query to one of them.
//!
//! Every terminal normally takes a pool, so multi-database work is
//! already possible by passing the right one. This module maps a name
//! to a pool once at startup, so a call site can say `.using("replica")`
//! instead of carrying a `Pool` around.
//!
//! ```ignore
//! // At startup (the `DATABASES` equivalent):
//! rustango::databases::register("default", primary_pool);
//! rustango::databases::register("replica", replica_pool);
//!
//! // Route a read to the replica:
//! let posts = Post::objects()
//!     .filter("published", true)
//!     .using("replica")
//!     .fetch()
//!     .await?;
//! ```
//!
//! `.using` offers read terminals only. That is on purpose: a write
//! must never go to a read replica by accident, so writes keep using
//! the explicit `fetch(&pool)` family. For automatic per-model
//! routing, see [`DatabaseRouter`](crate::databases::DatabaseRouter).

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::sql::Pool;

/// The usual alias for the primary connection.
pub const DEFAULT_ALIAS: &str = "default";

static REGISTRY: OnceLock<RwLock<HashMap<String, Pool>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, Pool>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Store a pool under `alias`, replacing any pool already there. Call
/// once per database at startup. Names other than `"default"` are
/// yours to pick.
pub fn register(alias: impl Into<String>, pool: impl Into<Pool>) {
    registry()
        .write()
        .expect("databases registry not poisoned")
        .insert(alias.into(), pool.into());
}

/// The pool for `alias`, or `None` if nothing is registered. Use
/// [`pool`] when the alias should always be there.
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

/// The pool for `alias`.
///
/// # Panics
/// If `alias` is not registered. That is a wiring bug at startup, and
/// failing loudly beats quietly using the wrong database.
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

/// Every registered alias, sorted.
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

/// Drop every registered connection. For test isolation.
pub fn clear() {
    registry()
        .write()
        .expect("databases registry not poisoned")
        .clear();
}

// ---- routers ----

/// Picks the alias a model's reads and writes should go to.
/// Use it for read replicas or sharding without naming an alias at
/// each call site.
///
/// Both methods return `None` by default, which means "no opinion".
/// Routers run in the order you register them and the first `Some`
/// wins; if all defer, the `"default"` alias is used.
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

/// Add a router to the end of the chain.
pub fn register_router(router: impl DatabaseRouter) {
    routers()
        .write()
        .expect("routers registry not poisoned")
        .push(Box::new(router));
}

/// Drop every registered router. For test isolation.
pub fn clear_routers() {
    routers()
        .write()
        .expect("routers registry not poisoned")
        .clear();
}

/// The alias to read `model` from: the first router that answers, or
/// `None` if they all defer. Callers then use [`DEFAULT_ALIAS`].
#[must_use]
pub fn route_read(model: &crate::core::ModelSchema) -> Option<String> {
    routers()
        .read()
        .expect("routers registry not poisoned")
        .iter()
        .find_map(|r| r.db_for_read(model))
}

/// The alias to write `model` to. See [`route_read`].
#[must_use]
pub fn route_write(model: &crate::core::ModelSchema) -> Option<String> {
    routers()
        .read()
        .expect("routers registry not poisoned")
        .iter()
        .find_map(|r| r.db_for_write(model))
}

/// The read pool for `model` from the router chain, or the
/// `"default"` alias.
///
/// # Panics
/// If the chosen alias is not registered. See [`pool`].
#[must_use]
pub fn read_pool_for(model: &crate::core::ModelSchema) -> Pool {
    pool(&route_read(model).unwrap_or_else(|| DEFAULT_ALIAS.to_owned()))
}

/// The write pool for `model`. See [`read_pool_for`].
///
/// # Panics
/// If the chosen alias is not registered. See [`pool`].
#[must_use]
pub fn write_pool_for(model: &crate::core::ModelSchema) -> Pool {
    pool(&route_write(model).unwrap_or_else(|| DEFAULT_ALIAS.to_owned()))
}

impl<T: crate::core::Model> crate::query::QuerySet<T> {
    /// Run this queryset against the connection registered under
    /// `alias`.
    ///
    /// The returned [`UsingQuerySet`] has read terminals only. Writes
    /// stay on the explicit `fetch(&pool)` family so one cannot reach
    /// a read replica by mistake.
    ///
    /// # Panics
    /// If `alias` is not registered. See [`pool`].
    #[must_use]
    pub fn using(self, alias: &str) -> UsingQuerySet<T> {
        UsingQuerySet {
            qs: self,
            pool: pool(alias),
        }
    }

    /// Let the [`DatabaseRouter`] chain pick the connection. The
    /// automatic version of [`Self::using`].
    ///
    /// Read terminals only, as with [`Self::using`]. For writes call
    /// `fetch(&write_pool_for(T::SCHEMA))`.
    ///
    /// # Panics
    /// If the chosen alias is not registered. See [`pool`].
    #[must_use]
    pub fn routed(self) -> UsingQuerySet<T> {
        let pool = read_pool_for(T::SCHEMA);
        UsingQuerySet { qs: self, pool }
    }
}

/// A queryset bound to one registered connection. Read terminals only.
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
    /// Run the query on the chosen connection.
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

    /// `SELECT COUNT(*)` on the chosen connection.
    ///
    /// # Errors
    /// As [`crate::sql::CounterPool::count`].
    pub async fn count(self) -> Result<i64, crate::sql::ExecError> {
        use crate::sql::CounterPool as _;
        self.qs.count(&self.pool).await
    }

    /// `EXISTS` on the chosen connection.
    ///
    /// # Errors
    /// As [`crate::sql::ExistsPool::exists`].
    pub async fn exists(self) -> Result<bool, crate::sql::ExecError> {
        use crate::sql::ExistsPool as _;
        self.qs.exists(&self.pool).await
    }
}
