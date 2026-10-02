//! Context that follows work off the request thread.
//!
//! `tokio::spawn` does not inherit `tokio::task_local!`, so a job runs
//! with none of the ambient context its caller had. An audit row written
//! from a job records `system` — so "who deleted this customer?" is a
//! dead end whenever a job did it.
//!
//! ## An explicit list, not every task-local
//!
//! It copies the task-locals named below, and only those. Declaring a
//! new `task_local!` does not opt it in.
//!
//! | task-local | copied? | why |
//! |---|---|---|
//! | `audit::AUDIT_SOURCE` (+ its tenant binding) | yes | who triggered the work |
//! | `i18n::timezone::ACTIVE_TZ` | yes | a formatting preference |
//! | `sql::executor::atomic::BLOCK` | **no** | the caller's live transaction and its on-commit queue |
//! | `signals::SUPPRESS_SIGNALS` | **no** | `save_quietly` must not silence a job days later |
//! | `admin::session::CURRENT_SESSION`, `CURRENT_CSRF_TOKEN` | **no** | belong to one request |
//! | `tenant_log::TENANT` | **no** | a log label for one request |
//!
//! A value that was never set is not copied as a default: the work sees
//! "no scope", exactly as its caller did.
//!
//! A source set by the tenant admin (or `audit::with_tenant_source`) names a user of that tenant, so it
//! is recorded only where writes are known to go there (a
//! `for_each_tenant` pass over it); elsewhere, the registry included,
//! it reads as `system`.
//!
//! ## Where it is carried
//!
//! - `jobs::InMemoryJobQueue` captures at `dispatch`.
//! - `scheduler::Scheduler` captures at `every()`: a tick has no caller,
//!   so registration is the hand-off.
//! - **`jobs::pg::PgJobQueue` does not**: its envelope is a
//!   `rustango_jobs` row, so it needs a column first (#1229). Treat a
//!   `System` source on its rows as "unknown", not "the framework".
//!
//! ## What the source does not tell you
//!
//! A job's audit row names the enqueuer and nothing else, so it reads
//! identically to one written on the request itself. Recording *that*
//! it ran deferred needs its own column rather than a prefix on the
//! token, which would break the exact-match filters that read it: #1385.

use crate::audit::{AuditSource, CapturedSource};

/// A snapshot of the ambient context worth carrying into deferred work.
///
/// Capture at the point work is *handed off* (enqueue), not where it
/// runs — a worker is spawned at boot and has no caller.
#[derive(Debug, Clone)]
pub struct TaskContext {
    source: Option<CapturedSource>,
    offset: Option<chrono::FixedOffset>,
}

impl TaskContext {
    /// Snapshot the current task's context.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            source: CapturedSource::capture(),
            offset: crate::i18n::timezone::active_override(),
        }
    }

    /// The audit source this context carries, if one was set.
    #[must_use]
    pub fn source(&self) -> Option<&AuditSource> {
        self.source.as_ref().map(CapturedSource::source)
    }

    /// Run `fut` with this context installed.
    pub async fn install<F, T>(self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let fut = async move {
            match self.offset {
                Some(offset) => crate::i18n::timezone::with_offset(offset, fut).await,
                None => fut.await,
            }
        };
        match self.source {
            Some(source) => source.scope(fut).await,
            None => fut.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{current_source, with_source, AuditSource};

    /// Work nobody triggered captures nothing, and installs nothing: no
    /// invented actor, and no explicit UTC override.
    #[tokio::test]
    async fn no_scope_stays_no_scope() {
        let ctx = TaskContext::capture();
        assert!(ctx.source().is_none());
        assert!(ctx.offset.is_none());
        let (token, tz) = ctx
            .install(async {
                (
                    current_source().as_token(),
                    crate::i18n::timezone::active_override(),
                )
            })
            .await;
        assert_eq!(token, "system");
        assert_eq!(tz, None);
    }

    #[tokio::test]
    async fn capture_takes_the_active_source() {
        with_source(AuditSource::User { id: "42".into() }, async {
            let ctx = TaskContext::capture();
            assert_eq!(
                ctx.source().map(AuditSource::as_token).as_deref(),
                Some("user:42")
            );
        })
        .await;
    }

    /// A tenant user's id is recorded only on writes known to go to that
    /// tenant; elsewhere it falls back to `system` (#1229).
    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn a_tenant_bound_source_stays_in_its_tenant() {
        use crate::audit::{with_tenant_source, writing_to_tenant};
        let ctx = with_tenant_source(AuditSource::User { id: "42".into() }, "a".into(), async {
            assert_eq!(current_source().as_token(), "user:42", "the request itself");
            TaskContext::capture()
        })
        .await;
        let tokens = tokio::spawn(ctx.install(async {
            let unknown = current_source().as_token();
            let in_a = writing_to_tenant("a".into(), async { current_source().as_token() }).await;
            let in_b = writing_to_tenant("b".into(), async { current_source().as_token() }).await;
            let explicit = with_source(AuditSource::Custom("cli".into()), async {
                writing_to_tenant("b".into(), async { current_source().as_token() }).await
            })
            .await;
            (unknown, in_a, in_b, explicit)
        }))
        .await
        .expect("join");
        assert_eq!(tokens.0, "system", "write target unknown");
        assert_eq!(tokens.1, "user:42");
        assert_eq!(tokens.2, "system", "tenant B must not get A's user id");
        assert_eq!(tokens.3, "cli", "an explicit source is not bound");
    }

    /// The whole point: a spawned task sees the captured context, which
    /// `tokio::spawn` alone would have dropped.
    #[tokio::test]
    async fn context_survives_a_spawn() {
        let ctx = with_source(AuditSource::User { id: "7".into() }, async {
            TaskContext::capture()
        })
        .await;

        let token =
            tokio::spawn(async move { ctx.install(async { current_source().as_token() }).await })
                .await
                .expect("join");

        assert_eq!(token, "user:7");
    }

    /// Without the context, a spawn is exactly the bug this exists for.
    #[tokio::test]
    async fn a_bare_spawn_still_loses_it() {
        let token = with_source(AuditSource::User { id: "9".into() }, async {
            tokio::spawn(async { current_source().as_token() })
                .await
                .expect("join")
        })
        .await;
        assert_eq!(token, "system", "bare spawn should NOT inherit");
    }

    /// A job dispatched from inside a job keeps the original actor, with
    /// no marker accumulating between the hops.
    #[tokio::test]
    async fn chaining_jobs_keeps_the_original_actor() {
        let first = with_source(AuditSource::User { id: "42".into() }, async {
            TaskContext::capture()
        })
        .await;

        let second = first.install(async { TaskContext::capture() }).await;

        assert_eq!(
            second.source().map(AuditSource::as_token).as_deref(),
            Some("user:42")
        );
    }
}
