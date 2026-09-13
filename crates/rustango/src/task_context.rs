//! Context that follows work off the request thread.
//!
//! `tokio::spawn` does not inherit `tokio::task_local!`, so a job runs
//! with none of the ambient context its caller had. An audit row written
//! from a job records `system` — so "who deleted this customer?" is a
//! dead end whenever a job did it.
//!
//! ## Why this is a list and not a loop
//!
//! Copying *every* task-local across the boundary is the obvious
//! implementation and it is wrong. Three of the framework's five would
//! cause bugs if they crossed:
//!
//! | task-local | crosses? | why |
//! |---|---|---|
//! | `AUDIT_SOURCE` | yes | who triggered the work — the point of this |
//! | `ACTIVE_TZ` | yes | a formatting preference; harmless and useful |
//! | `ON_COMMIT` | **no** | a callback queue for a *live* transaction. A job inheriting it would queue callbacks onto one that already finished. |
//! | `SUPPRESS_SIGNALS` | **no** | means "don't fire signals *here*". A request calling `save_quietly` must not silence a job running days later. |
//! | `CURRENT_SESSION` | **no** | a *request's* session. A job is not in a request; deep-stack helpers would act on a stale one. |
//!
//! So adding to this list is a deliberate act. Declaring a new
//! `task_local!` elsewhere does not opt it in.
//!
//! ## What does not carry this yet
//!
//! Only `jobs::InMemoryJobQueue` installs a captured context. Two other
//! spawn boundaries still drop everything, and a job or task running on
//! them sees `System` and UTC exactly as before:
//!
//! - **`jobs::pg::PgJobQueue`** — its envelope is a `rustango_jobs` row,
//!   not a struct in memory, so carrying a context needs a column and a
//!   migration. This is the queue most production deployments run, so
//!   the gap is the larger half of #1229, not a corner case.
//! - **`scheduler`** — a tick has no enqueuer to inherit from,
//!   so it needs a context *assigned* rather than captured.
//!
//! Until both land, "who did this?" is answerable for in-memory jobs
//! only. Treat a `System` source on a job row as "unknown", not as
//! "the framework".
//!
//! ## What the source does not tell you
//!
//! A job's audit row names the enqueuer and nothing else, so it reads
//! identically to one written on the request itself. Recording *that*
//! it ran deferred needs its own column rather than a prefix on the
//! token, which would break the exact-match filters that read it: #1385.

use crate::audit::AuditSource;

/// A snapshot of the ambient context worth carrying into deferred work.
///
/// Capture at the point work is *handed off* (enqueue), not where it
/// runs — a worker is spawned at boot and has no caller.
#[derive(Debug, Clone)]
pub struct TaskContext {
    source: AuditSource,
    offset: chrono::FixedOffset,
}

impl TaskContext {
    /// Snapshot the current task's context.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            source: crate::audit::current_source(),
            offset: crate::i18n::timezone::current_offset(),
        }
    }

    /// The audit source this context carries.
    #[must_use]
    pub fn source(&self) -> &AuditSource {
        &self.source
    }

    /// Run `fut` with this context installed.
    pub async fn install<F, T>(self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let offset = self.offset;
        crate::audit::with_source(self.source, crate::i18n::timezone::with_offset(offset, fut))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{current_source, with_source, AuditSource};

    /// Work nobody triggered — a boot hook, a test — captures as
    /// `System`/UTC rather than inventing an actor or a locale.
    #[tokio::test]
    async fn capture_outside_any_scope_is_system_utc() {
        let ctx = TaskContext::capture();
        assert_eq!(ctx.source().as_token(), "system");
        assert_eq!(ctx.offset, chrono::FixedOffset::east_opt(0).unwrap());
    }

    #[tokio::test]
    async fn capture_takes_the_active_source() {
        with_source(AuditSource::User { id: "42".into() }, async {
            assert_eq!(TaskContext::capture().source().as_token(), "user:42");
        })
        .await;
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

        assert_eq!(second.source().as_token(), "user:42");
    }
}
