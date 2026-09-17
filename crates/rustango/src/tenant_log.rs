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
    // The slot is the dedupe, so it has to be consulted *first*.
    //
    // These two halves used to run in the other order, with the
    // `span.record` calls above an unguarded `OnceLock::set`. "First
    // call wins" was then true of the slot and false of the span: every
    // resolver in `ChainResolver` that published the same tenant
    // stamped the field again. Measured on a running app, `/healthz`
    // rendered `tenant=` three times — and `/healthz` is usually the
    // highest-volume endpoint in a deployment. Cosmetic in text mode; a
    // duplicate key in JSON, which is the format that ships to a
    // collector.
    //
    // `Ok(false)` means the slot was already filled — a later resolver,
    // so nothing to do. `Err` means no scope is open (a CLI verb, a
    // test), and there is no slot to dedupe against; record
    // unconditionally, as before.
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

    /// One `tenant=` per line, however many resolvers run.
    ///
    /// `record`'s doc says "First call wins". That holds for the slot —
    /// `OnceLock::set` fails silently on repeat — but the two
    /// `span.record` calls sat above that check, unguarded, and fired
    /// on every `ChainResolver::resolve`. Paths that resolve more than
    /// once stamped the field repeatedly: measured on a booted CMS,
    /// `/healthz` rendered `tenant=` three times, `/` three, `/login`
    /// twice. Cosmetic in text mode; a duplicate key in JSON, which is
    /// what ships to a collector.
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
            // Three resolvers in the chain all publish the same tenant.
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
