//! Custom bulk actions: the context a handler gets, and its type.

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::request::Parts;
use axum::http::Method;

use crate::core::SqlValue;
use crate::object_permissions::Actor;
use crate::sql::PoolTx;

use super::urls::AdminActionFuture;

/// A custom bulk action handler; register it with
/// [`Builder::register_action`](super::Builder::register_action).
pub type ActionFn = Arc<dyn for<'a> Fn(ActionCtx<'a>) -> AdminActionFuture<'a> + Send + Sync>;

/// What a custom bulk action runs with: the admin's write transaction,
/// and the rows and principal the admin's checks approved.
///
/// ```no_run
/// use rustango::admin::{ActionCtx, ActionPerm, AdminActionFuture, AdminError, Builder};
/// use rustango::core::{Op, SqlValue};
/// use rustango::sql::Pool;
///
/// #[derive(rustango::Model)]
/// #[rustango(table = "post")]
/// pub struct Post {
///     #[rustango(primary_key)]
///     pub id: i64,
///     pub published_at: Option<chrono::DateTime<chrono::Utc>>,
/// }
///
/// fn publish(mut ctx: ActionCtx<'_>) -> AdminActionFuture<'_> {
///     Box::pin(async move {
///         let query = Post::objects()
///             .filter_op("id", Op::In, SqlValue::List(ctx.pks().to_vec()))
///             .update()
///             .set("published_at", chrono::Utc::now())
///             .compile()
///             .map_err(|e| AdminError::Internal(e.to_string()))?;
///         rustango::sql::update_tx(ctx.tx(), &query).await?;
///         Ok(())
///     })
/// }
///
/// fn admin(pool: Pool) -> Builder {
///     Builder::new(pool).register_action("post", "publish", ActionPerm::Change, publish)
/// }
/// # fn main() {}
/// ```
#[non_exhaustive]
pub struct ActionCtx<'a> {
    tx: &'a mut PoolTx<'static>,
    pks: &'a [SqlValue],
    actor: &'a Actor,
    #[cfg(feature = "tenancy")]
    org: Option<&'a crate::tenancy::Org>,
    parts: &'a Parts,
}

impl<'a> ActionCtx<'a> {
    pub(crate) fn new(
        tx: &'a mut PoolTx<'static>,
        pks: &'a [SqlValue],
        actor: &'a Actor,
        #[cfg(feature = "tenancy")] org: Option<&'a crate::tenancy::Org>,
        parts: &'a Parts,
    ) -> Self {
        Self {
            tx,
            pks,
            actor,
            #[cfg(feature = "tenancy")]
            org,
            parts,
        }
    }

    /// The admin's write transaction; the tenant's under the tenant admin.
    /// The action's audit rows commit or roll back with it.
    pub fn tx(&mut self) -> &mut PoolTx<'static> {
        self.tx
    }

    /// Keys of the selected rows that passed the permission and
    /// object-permission checks: act on these only.
    #[must_use]
    pub fn pks(&self) -> &'a [SqlValue] {
        self.pks
    }

    /// The principal the admin's checks ran for.
    #[must_use]
    pub fn actor(&self) -> &'a Actor {
        self.actor
    }

    /// The tenant, under the tenant admin; `None` in a bare admin.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn org(&self) -> Option<&'a crate::tenancy::Org> {
        self.org
    }

    /// The request method.
    #[must_use]
    pub fn method(&self) -> &'a Method {
        &self.parts.method
    }

    /// The request path, without the query.
    #[must_use]
    pub fn path(&self) -> &'a str {
        self.parts.uri.path()
    }

    /// The client address, as the rate limiter sees it.
    #[must_use]
    pub fn client_ip(&self) -> Option<IpAddr> {
        crate::real_ip::client_ip(&self.parts.extensions, &self.parts.headers)
    }

    /// The raw request parts, cookies and `Authorization` included.
    /// Never log them; prefer the accessors above.
    #[must_use]
    pub fn raw_request_parts_with_credentials(&self) -> &'a Parts {
        self.parts
    }
}

/// Adapt a `(tx, pks)` handler of the deprecated `register_tx_action*`.
pub(crate) fn from_tx_handler<F>(
    handler: F,
) -> impl for<'a> Fn(ActionCtx<'a>) -> AdminActionFuture<'a> + Send + Sync + 'static
where
    F: for<'a> Fn(&'a mut PoolTx<'static>, &'a [SqlValue]) -> AdminActionFuture<'a>
        + Send
        + Sync
        + 'static,
{
    move |ctx: ActionCtx<'_>| handler(ctx.tx, ctx.pks)
}
