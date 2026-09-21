//! Send email off the request path via the [`crate::jobs`] queue.
//!
//! Render the message in the request, push it on the queue, and let a
//! worker talk to SMTP, SES or Mailgun. SMTP is slow and flaky, so
//! this keeps handler latency steady and gets retry with backoff from
//! the queue.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::email_jobs::{EmailJobConfig, register_email_job, dispatch_email};
//! use rustango::jobs::JobQueue;
//! use rustango::email::BoxedMailer;
//! use std::sync::Arc;
//!
//! // Once at startup — register the worker handler with the queue.
//! let mailer: BoxedMailer = Arc::new(SmtpMailer::new(...));
//! register_email_job(&queue, EmailJobConfig::new(mailer.clone())).await;
//! queue.start().await;
//!
//! // From a handler:
//! let email = renderer.render("welcome", &ctx)?
//!     .from("noreply@example.com")
//!     .to("alice@example.com");
//! dispatch_email(&queue, &email).await?;
//! ```
//!
//! ## Behavior
//!
//! - The whole [`Email`] goes into the job payload, so the worker
//!   rebuilds it before sending.
//! - A send failure becomes [`crate::jobs::JobError::Retryable`], so
//!   the queue backs off and retries. A job that runs out of attempts
//!   goes to the dead-letter callback.
//! - The worker reads the [`crate::email::Mailer`] from a static
//!   registry keyed by job name. Registering again replaces it, which
//!   is handy in tests.
//!
//! [`Email`]: crate::email::Email

use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::email::{BoxedMailer, Email};
use crate::jobs::{Job, JobError, JobQueue};

/// Mailers by [`Job::NAME`]. The worker looks one up here so the
/// mailer stays out of the job payload, which would need
/// `Mailer: Serialize`.
fn mailer_registry() -> &'static RwLock<std::collections::HashMap<&'static str, BoxedMailer>> {
    static REG: OnceLock<RwLock<std::collections::HashMap<&'static str, BoxedMailer>>> =
        OnceLock::new();
    REG.get_or_init(|| RwLock::new(std::collections::HashMap::new()))
}

/// Config registered together with the job.
#[derive(Clone)]
pub struct EmailJobConfig {
    /// The mailer the worker sends with.
    pub mailer: BoxedMailer,
}

impl EmailJobConfig {
    #[must_use]
    pub fn new(mailer: BoxedMailer) -> Self {
        Self { mailer }
    }
}

/// The job payload: a copy of the [`Email`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailJob {
    pub email: Email,
}

#[async_trait::async_trait]
impl Job for EmailJob {
    const NAME: &'static str = "rustango.send_email";
    /// 5 attempts at the queue's `1s · 2^attempt` backoff, about a
    /// minute in all.
    const MAX_ATTEMPTS: u32 = 5;

    async fn run(&self) -> Result<(), JobError> {
        let mailer = mailer_registry()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(Self::NAME)
            .cloned()
            .ok_or_else(|| {
                JobError::Queue(
                    "EmailJob: no mailer registered (call register_email_job at startup)".into(),
                )
            })?;
        mailer
            .send(&self.email)
            .await
            .map_err(|e| JobError::Retryable(format!("mailer: {e}")))
    }
}

/// Register the email job and its mailer on `queue`, once at startup.
/// Calling it again replaces the mailer.
pub async fn register_email_job<Q: JobQueue>(queue: &Q, cfg: EmailJobConfig) {
    mailer_registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(EmailJob::NAME, cfg.mailer);
    queue.register::<EmailJob>().await;
}

/// Queue an email and return at once; a worker delivers it.
///
/// # Errors
/// [`JobError::Queue`] when the enqueue fails, for example the
/// database is down or the payload will not serialize.
pub async fn dispatch_email<Q: JobQueue>(queue: &Q, email: &Email) -> Result<(), JobError> {
    queue
        .dispatch(&EmailJob {
            email: email.clone(),
        })
        .await
}

/// Clear the mailer registry between tests.
#[cfg(test)]
pub fn reset_mailer_registry() {
    if let Ok(mut g) = mailer_registry().write() {
        g.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::{InMemoryMailer, NullMailer};
    use crate::jobs::InMemoryJobQueue;
    use std::sync::Arc as StdArc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    /// These tests share the global mailer registry, so they must run
    /// one at a time.
    fn lock() -> &'static Mutex<()> {
        static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    fn email() -> Email {
        Email::new()
            .from("noreply@x.com")
            .to("alice@x.com")
            .subject("Hi")
            .body("hello")
    }

    #[tokio::test]
    async fn dispatch_then_worker_sends() {
        let _g = lock().lock().await;
        reset_mailer_registry();
        let mailer = StdArc::new(InMemoryMailer::new());
        let q = InMemoryJobQueue::with_workers(1);
        register_email_job(&q, EmailJobConfig::new(mailer.clone())).await;
        q.start().await;

        dispatch_email(&q, &email()).await.unwrap();

        for _ in 0..50 {
            if mailer.count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(mailer.count(), 1);
        let sent = mailer.sent();
        assert_eq!(sent[0].subject, "Hi");
        assert_eq!(sent[0].to, vec!["alice@x.com"]);

        q.shutdown().await;
    }

    #[tokio::test]
    async fn no_mailer_registered_returns_queue_error() {
        let _g = lock().lock().await;
        reset_mailer_registry();
        // Register the job so dispatch works, but no mailer.
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<EmailJob>().await;

        // Count dead letters to see the worker reject the job.
        let dl_count = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
        let dl = dl_count.clone();
        q.on_dead_letter(move |_dl| {
            let dl = dl.clone();
            async move {
                dl.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .await;
        q.start().await;

        dispatch_email(&q, &email()).await.unwrap();

        // JobError::Queue is not retried, so it dead-letters at once.
        for _ in 0..40 {
            if dl_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(dl_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        q.shutdown().await;
    }

    #[tokio::test]
    async fn re_register_swaps_the_mailer() {
        let _g = lock().lock().await;
        reset_mailer_registry();
        let m1 = StdArc::new(InMemoryMailer::new());
        let m2 = StdArc::new(InMemoryMailer::new());
        let q = InMemoryJobQueue::with_workers(1);

        register_email_job(&q, EmailJobConfig::new(m1.clone())).await;
        register_email_job(&q, EmailJobConfig::new(m2.clone())).await;
        q.start().await;

        dispatch_email(&q, &email()).await.unwrap();
        for _ in 0..50 {
            if m2.count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(m1.count(), 0, "old mailer should not receive");
        assert_eq!(m2.count(), 1, "new mailer should receive");

        q.shutdown().await;
    }

    #[tokio::test]
    async fn null_mailer_succeeds_silently() {
        let _g = lock().lock().await;
        reset_mailer_registry();
        let mailer: BoxedMailer = StdArc::new(NullMailer);
        let q = InMemoryJobQueue::with_workers(1);
        register_email_job(&q, EmailJobConfig::new(mailer)).await;
        q.start().await;

        // Nothing to observe: it must not panic or dead-letter.
        dispatch_email(&q, &email()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Back to 0 once the job is done.
        assert_eq!(q.pending_count().await, 0);
        q.shutdown().await;
    }
}
