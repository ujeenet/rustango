//! The one place that picks a request's tenant context (#1826). Auth,
//! sessions and every extractor read it, so with two contexts mounted
//! they still agree on one tenant.

use std::any::Any;
use std::sync::Arc;

use axum::http::request::Parts;
use axum::http::Extensions;
use sqlx::Database;

use crate::sql::{sqlx, Pool};
use crate::tenancy::middleware::SessionKeys;
use crate::tenancy::{Org, OrgResolver as _, TenancyError};

use super::{DatabaseTenantContext, TenantContext};

/// The tenant context this request uses: the first mounted one, in
/// [`MountedTenantContext::of`]'s order.
#[derive(Clone, Copy)]
pub(crate) enum MountedTenantContext<'a> {
    #[cfg(feature = "postgres")]
    Pg(&'a Arc<TenantContext<sqlx::Postgres>>),
    #[cfg(feature = "sqlite")]
    Sqlite(&'a Arc<TenantContext<sqlx::Sqlite>>),
    #[cfg(feature = "mysql")]
    MySql(&'a Arc<TenantContext<sqlx::MySql>>),
    #[cfg(feature = "postgres")]
    DbPg(&'a Arc<DatabaseTenantContext<sqlx::Postgres>>),
    #[cfg(feature = "sqlite")]
    DbSqlite(&'a Arc<DatabaseTenantContext<sqlx::Sqlite>>),
    #[cfg(feature = "mysql")]
    DbMySql(&'a Arc<DatabaseTenantContext<sqlx::MySql>>),
}

/// Run `$full` on a `TenantContext` arm and `$db` on a
/// `DatabaseTenantContext` arm, with the context bound to `$c`.
macro_rules! each {
    ($self:expr, |$c:ident| full => $full:expr, db => $db:expr) => {
        match $self {
            #[cfg(feature = "postgres")]
            MountedTenantContext::Pg($c) => $full,
            #[cfg(feature = "sqlite")]
            MountedTenantContext::Sqlite($c) => $full,
            #[cfg(feature = "mysql")]
            MountedTenantContext::MySql($c) => $full,
            #[cfg(feature = "postgres")]
            MountedTenantContext::DbPg($c) => $db,
            #[cfg(feature = "sqlite")]
            MountedTenantContext::DbSqlite($c) => $db,
            #[cfg(feature = "mysql")]
            MountedTenantContext::DbMySql($c) => $db,
        }
    };
}

impl<'a> MountedTenantContext<'a> {
    /// The context mounted on `ext`, or `None` when there is none.
    pub(crate) fn of(ext: &'a Extensions) -> Option<Self> {
        macro_rules! pick {
            ($variant:ident, $ctx:ty) => {
                if let Some(c) = ext.get::<Arc<$ctx>>() {
                    return Some(Self::$variant(c));
                }
            };
        }
        #[cfg(feature = "postgres")]
        pick!(Pg, TenantContext<sqlx::Postgres>);
        #[cfg(feature = "sqlite")]
        pick!(Sqlite, TenantContext<sqlx::Sqlite>);
        #[cfg(feature = "mysql")]
        pick!(MySql, TenantContext<sqlx::MySql>);
        #[cfg(feature = "postgres")]
        pick!(DbPg, DatabaseTenantContext<sqlx::Postgres>);
        #[cfg(feature = "sqlite")]
        pick!(DbSqlite, DatabaseTenantContext<sqlx::Sqlite>);
        #[cfg(feature = "mysql")]
        pick!(DbMySql, DatabaseTenantContext<sqlx::MySql>);
        None
    }

    fn erased(self) -> &'a dyn Any {
        each!(self, |c| full => c as &dyn Any, db => c as &dyn Any)
    }

    /// This context, if it is a `TenantContext<DB>`.
    pub(crate) fn full<DB: Database>(self) -> Option<&'a Arc<TenantContext<DB>>> {
        self.erased().downcast_ref()
    }

    /// This context, if it is a `DatabaseTenantContext<DB>`.
    pub(crate) fn database<DB: Database>(self) -> Option<&'a Arc<DatabaseTenantContext<DB>>> {
        self.erased().downcast_ref()
    }

    /// Resolve the request's Org against this context's registry.
    pub(crate) async fn resolve(self, parts: &Parts) -> Result<Option<Org>, TenancyError> {
        each!(self, |c|
            full => c.resolver.resolve(parts, &c.pools.registry_pool()).await,
            db => c.resolver.resolve(parts, &c.registry).await)
    }

    /// The signing keys and registry pool of this context.
    pub(crate) fn session_keys(self) -> SessionKeys<'a> {
        each!(self, |c|
        full => SessionKeys {
            session: &c.session_secret,
            operator: &c.operator_secret,
            registry: c.pools.registry_pool(),
        },
        db => SessionKeys {
            session: &c.session_secret,
            operator: &c.operator_secret,
            registry: c.registry.clone(),
        })
    }

    /// `org`'s data pool from this context.
    pub(crate) async fn pool_for(self, org: &Org) -> Result<Pool, TenancyError> {
        each!(self, |c|
            full => c.pools.scoped_pool_dyn(org).await,
            db => c.pools.pool_for_org(org).await.map(|p| Pool::from(p.pool().clone())))
    }
}
