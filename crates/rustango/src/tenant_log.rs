//! Ambient tenant identity for log lines — the field that says *whose*
//! request this was.
//!
//! Tenant identity lives in the request: `Tenant` is an axum extractor,
//! not a task-local ([`crate::extractors::tenant`]). By the time anything
//! logs, the resolved [`Org`](crate::tenancy::Org) has been moved into a
//! handler and nothing downstream can see it — which is why an access-log
//! line could name the method, path, status and IP, but never the tenant.
//!
//! This module is the missing slot. [`scope`] opens one per request,
//! the resolver calls [`record`] when it identifies the tenant, and
//! outer middleware reads it back with [`current`] after the handler
//! returns.
//!
//! ```ignore
//! // Middleware, outside the handler. Read back INSIDE the scope:
//! // once the `scope` future resolves the slot is gone, and a read
//! // after it returns `None` for every request.
//! let (response, who) = tenant_log::scope(async {
//!     let response = next.run(req).await;
//!     (response, tenant_log::current())   // Some(acme)
//! })
//! .await;
//! ```
//!
//! [`record`] also writes `tenant` / `org_id` onto the current `tracing`
//! span, so with [`crate::tracing_layer::TracingLayer`] installed every
//! event emitted during the request carries the tenant in its span
//! context — the ORM's included, with no plumbing of their own.
//!
//! ## Scope
//!
//! Per-task, and `tokio::spawn` does not inherit it: work that leaves the
//! request leaves the scope. Giving background jobs a tenant is a
//! different mechanism (issues #1229 / #1223) — this covers the request
//! path only.

use std::future::Future;
use std::sync::{Arc, OnceLock};

/// The resolved tenant, as logs should name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantLabel {
    /// `Org.slug` — operator-chosen, and often a customer name. See
    /// [`crate::access_log::TenantField`] for logging the id instead.
    pub slug: String,
    /// `Org.id`. `None` for an unsaved org, which no request path
    /// produces — the field is Optional because `Auto<i64>` is.
    pub id: Option<i64>,
}

tokio::task_local! {
    /// Per-request slot for the resolved tenant.
    ///
    /// `Arc<OnceLock<_>>` rather than a plain task-local value because
    /// the write happens *below* the read: the resolver runs inside the
    /// handler, the middleware that logs sits outside it. A task-local
    /// holding a plain value could not be filled in from in there.
    /// Set-once matches the semantic — one request, one tenant.
    static TENANT: Arc<OnceLock<TenantLabel>>;
}

/// Run `fut` with a fresh tenant slot in scope.
///
/// Nesting is safe: an inner call reuses the outer slot rather than
/// shadowing it, so two layers that both open a scope still agree on
/// one tenant per request.
pub async fn scope<F, T>(fut: F) -> T
where
    F: Future<Output = T>,
{
    if TENANT.try_with(|_| ()).is_ok() {
        return fut.await;
    }
    TENANT.scope(Arc::new(OnceLock::new()), fut).await
}

/// Publish the request's tenant. Called by the tenant resolver the
/// moment an `Org` is identified.
///
/// Records `tenant` / `org_id` on the current span, and fills the
/// [`scope`] slot when one is open. Both halves are no-ops outside
/// their respective contexts, so calling this from a CLI verb or a
/// test costs nothing and breaks nothing. First call wins.
pub fn record(slug: &str, id: Option<i64>) {
    let span = tracing::Span::current();
    span.record("tenant", slug);
    if let Some(id) = id {
        span.record("org_id", id);
    }
    let _ = TENANT.try_with(|slot| {
        slot.set(TenantLabel {
            slug: slug.to_owned(),
            id,
        })
    });
}

/// The tenant resolved for the current request, if any.
///
/// `None` means no tenant was identified — an apex-domain or operator
/// console request, a single-tenant deployment, or any code running
/// outside a [`scope`]. It never means "there was one and we lost it".
#[must_use]
pub fn current() -> Option<TenantLabel> {
    TENANT.try_with(|slot| slot.get().cloned()).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_and_reads_back_inside_a_scope() {
        let seen = scope(async {
            record("acme", Some(7));
            current()
        })
        .await;
        assert_eq!(
            seen,
            Some(TenantLabel {
                slug: "acme".to_owned(),
                id: Some(7),
            })
        );
    }

    #[tokio::test]
    async fn current_is_none_without_a_resolution() {
        let seen = scope(async { current() }).await;
        assert_eq!(seen, None, "an apex request resolves no tenant");
    }

    #[tokio::test]
    async fn record_outside_a_scope_is_a_noop() {
        record("acme", Some(7));
        assert_eq!(current(), None);
    }

    #[tokio::test]
    async fn first_resolution_wins() {
        let seen = scope(async {
            record("acme", Some(1));
            record("globex", Some(2));
            current()
        })
        .await;
        assert_eq!(seen.unwrap().slug, "acme");
    }

    /// The slot does not outlive its scope. Pins the shape that read
    /// `tenant=-` on every line until the read moved inside — the
    /// failure is silent, so it needs a test of its own.
    #[tokio::test]
    async fn a_read_after_the_scope_closes_sees_nothing() {
        scope(async { record("acme", Some(7)) }).await;
        assert_eq!(current(), None);
    }

    /// Two layers both opening a scope must not shadow each other —
    /// otherwise the resolver fills the inner slot and the outer
    /// middleware reads an empty one.
    #[tokio::test]
    async fn a_nested_scope_reuses_the_outer_slot() {
        let seen = scope(async {
            scope(async { record("acme", Some(7)) }).await;
            current()
        })
        .await;
        assert_eq!(seen.map(|t| t.slug), Some("acme".to_owned()));
    }

    /// A spawned task is a different task: it neither sees the parent's
    /// tenant nor leaks one back. Pins the boundary #1223 has to cross.
    #[tokio::test]
    async fn a_spawned_task_does_not_inherit_the_scope() {
        let seen = scope(async {
            record("acme", Some(7));
            tokio::spawn(async { current() }).await.unwrap()
        })
        .await;
        assert_eq!(seen, None);
    }
}
