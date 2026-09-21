//! Tenant identity for log lines: the field that says whose request
//! this was.
//!
//! `Tenant` is an axum extractor, so the resolved
//! [`Org`](crate::tenancy::Org) is moved into the handler and the
//! logging middleware outside it cannot see the tenant. This module is
//! the missing slot. [`scope`] opens one per request, the resolver
//! calls [`record`], and outer middleware reads it with [`current`].
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
//! [`record`] also puts `tenant` and `org_id` on the current `tracing`
//! span, so with [`crate::tracing_layer::TracingLayer`] installed every
//! event in the request carries the tenant, the ORM's events included.
//!
//! ## Scope and safety
//!
//! The slot is per-task and set once. `tokio::spawn` does not inherit
//! it, so work that leaves the request leaves the scope. Keep it that
//! way: a slot shared or reused between requests would label one
//! tenant's log lines with another tenant's slug, which leaks customer
//! names across tenants and corrupts any per-tenant audit trail. Open a
//! fresh [`scope`] per request and never hold a [`TenantLabel`] past
//! it. Background jobs need their own mechanism.
//!
//! [`scope`]: crate::tenant_log::scope
//! [`record`]: crate::tenant_log::record
//! [`current`]: crate::tenant_log::current
//! [`TenantLabel`]: crate::tenant_log::TenantLabel

use std::future::Future;
use std::sync::{Arc, OnceLock};

/// The resolved tenant, as logs should name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantLabel {
    /// `Org.slug`. Often a customer name, so treat logs holding it as
    /// customer data. See [`crate::access_log::TenantField`] to log the
    /// id instead.
    pub slug: String,
    /// `Org.id`. `None` only for an unsaved org, because `Auto<i64>` is
    /// optional; no request path produces that.
    pub id: Option<i64>,
}

tokio::task_local! {
    /// Per-request slot for the resolved tenant.
    ///
    /// It holds `Arc<OnceLock<_>>`, not a plain value, because the write
    /// happens below the read: the resolver runs inside the handler and
    /// the logging middleware sits outside it. Set-once matches the
    /// rule of one request, one tenant.
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

/// Publish the request's tenant. The resolver calls this as soon as it
/// identifies an `Org`.
///
/// Fills the [`scope`] slot if one is open, and puts `tenant` and
/// `org_id` on the current span. Both halves do nothing outside their
/// context, so a CLI verb or test can call it safely. First call wins,
/// so a later resolver cannot relabel the request.
pub fn record(slug: &str, id: Option<i64>) {
    // Check the slot first: it is what stops a chain of resolvers from
    // stamping the same field once each, which is a duplicate key in
    // JSON logs.
    //
    // `Ok(false)` means a resolver already filled it, so stop. `Err`
    // means no scope is open, so there is nothing to dedupe against and
    // we record anyway.
    let first = TENANT
        .try_with(|slot| {
            slot.set(TenantLabel {
                slug: slug.to_owned(),
                id,
            })
            .is_ok()
        })
        .unwrap_or(true);
    if !first {
        return;
    }

    let span = tracing::Span::current();
    span.record("tenant", slug);
    if let Some(id) = id {
        span.record("org_id", id);
    }
}

/// The tenant resolved for the current request, if any.
///
/// `None` means no tenant was identified: an apex-domain or operator
/// console request, a single-tenant deployment, or code outside a
/// [`scope`]. It never means one was found and then lost.
#[must_use]
pub fn current() -> Option<TenantLabel> {
    TENANT.try_with(|slot| slot.get().cloned()).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `tenant=` per line, however many resolvers run. More than
    /// one is a duplicate key in JSON logs.
    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn a_field_is_stamped_once_however_many_resolvers_run() {
        use std::io::Write;
        use std::sync::{Arc as StdArc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Buf(StdArc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _g = tracing::subscriber::set_default(subscriber);

        scope(async {
            let span = tracing::info_span!(
                "http.request",
                tenant = tracing::field::Empty,
                org_id = tracing::field::Empty,
            );
            let _e = span.enter();
            // Three resolvers publish the same tenant.
            record("acme", Some(7));
            record("acme", Some(7));
            record("acme", Some(7));
            tracing::info!("handled");
        })
        .await;

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("handled"), "nothing rendered:\n{out}");
        assert_eq!(
            out.matches("tenant=").count(),
            1,
            "tenant was stamped onto the span once per resolver instead of once \
             per request:\n{out}"
        );
        assert_eq!(
            out.matches("org_id=").count(),
            1,
            "org_id was stamped more than once:\n{out}"
        );
    }

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

    /// The slot does not outlive its scope. A read placed after it
    /// fails silently, so this needs its own test.
    #[tokio::test]
    async fn a_read_after_the_scope_closes_sees_nothing() {
        scope(async { record("acme", Some(7)) }).await;
        assert_eq!(current(), None);
    }

    /// Two layers opening a scope must not shadow each other, or the
    /// resolver fills the inner slot and the outer read comes back
    /// empty.
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
    /// tenant nor leaks one back.
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
