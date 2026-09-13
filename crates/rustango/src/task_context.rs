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

    /// Mark the source as having crossed into deferred work.
    ///
    /// `user:42` becomes `job:user:42`. The distinction is not
    /// decoration: a job enqueued on Monday and retried on Thursday
    /// would otherwise record that user 42 acted on Thursday, when they
    /// were not there. Attribution and presence are different claims,
    /// and a confidently wrong audit row is worse than a blank one.
    ///
    /// Uses the existing `Custom` variant, so `AuditSource` keeps its
    /// shape and nothing downstream has to match a new one.
    #[must_use]
    pub fn deferred(self) -> Self {
        let token = self.source.as_token();
        Self {
            source: AuditSource::Custom(format!("job:{token}")),
            ..self
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

impl Default for TaskContext {
    /// The context of work nobody triggered — a scheduled task, a boot
    /// hook. `System`, UTC.
    fn default() -> Self {
        Self {
            source: AuditSource::System,
            offset: chrono::FixedOffset::east_opt(0).expect("UTC is a valid offset"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{current_source, with_source, AuditSource};

    #[tokio::test]
    async fn capture_outside_any_scope_is_system() {
        let ctx = TaskContext::capture();
        assert_eq!(ctx.source().as_token(), "system");
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

    #[tokio::test]
    async fn deferred_marks_the_boundary_without_losing_the_actor() {
        let ctx = with_source(AuditSource::User { id: "42".into() }, async {
            TaskContext::capture()
        })
        .await
        .deferred();
        assert_eq!(ctx.source().as_token(), "job:user:42");
    }

    #[tokio::test]
    async fn deferred_system_stays_readable() {
        assert_eq!(
            TaskContext::capture().deferred().source().as_token(),
            "job:system"
        );
    }

    #[tokio::test]
    async fn default_is_system_utc() {
        let ctx = TaskContext::default();
        assert_eq!(ctx.source().as_token(), "system");
        assert_eq!(ctx.offset, chrono::FixedOffset::east_opt(0).unwrap());
    }
}
