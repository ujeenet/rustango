//! Send email off the request path via the [`crate::jobs`] queue.
//!
//! Every framework with both an email layer and a job queue ends up
//! reinventing this glue: render the message inside a request, push
//! the rendered envelope onto a queue, and let a worker actually
//! talk to the SMTP/SES/Mailgun backend. Doing that on a per-request
//! basis keeps handler latency predictable (SMTP can be slow + flaky)
//! and gets you free retry-with-backoff via the queue.
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
//! - The full [`Email`] (subject, bodies, recipients, headers) is
//!   serialized into the queue payload, so the worker re-creates it
//!   before sending.
//! - Send failures bubble up as [`crate::jobs::JobError::Retryable`],
//!   so the queue's exponential backoff handles transient SMTP
//!   issues. Permanent failures (invalid address etc.) are logged by
//!   the dead-letter callback.
//! - The worker pulls the [`crate::email::Mailer`] from a static
//!   registry keyed by job name. Re-registering replaces the
//!   previous mailer (handy for tests).

use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::email::{BoxedMailer, Email};
use crate::jobs::{Job, JobError, JobQueue};

/// Static registry of mailers, keyed by [`Job::NAME`]. Lets the
/// worker grab the Mailer without it being part of the queue
/// payload (which would force `Mailer: Serialize`, defeating the
/// trait-object pattern).
fn mailer_registry() -> &'static RwLock<std::collections::HashMap<&'static str, BoxedMailer>> {
    static REG: OnceLock<RwLock<std::collections::HashMap<&'static str, BoxedMailer>>> =
        OnceLock::new();
    REG.get_or_init(|| RwLock::new(std::collections::HashMap::new()))
}

/// Per-app config carried alongside the registered job.
#[derive(Clone)]
pub struct EmailJobConfig {
    /// Mailer used by the worker to actually send.
    pub mailer: BoxedMailer,
}

impl EmailJobConfig {
    #[must_use]
    pub fn new(mailer: BoxedMailer) -> Self {
        Self { mailer }
    }
}

/// The job payload — a serializable snapshot of an [`Email`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailJob {
    pub email: Email,
}

#[async_trait::async_trait]
impl Job for EmailJob {
    const NAME: &'static str = "rustango.send_email";
    /// Email senders typically retry for a while — 5 attempts at the
    /// queue's `1s · 2^attempt` backoff covers ~1 minute total.
    const MAX_ATTEMPTS: u32 = 5;

    async fn run(&self) -> Result<(), JobError> {
        let mailer = mailer_registry()
            .read()
            .expect("EmailJob registry poisoned")
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

/// Register the email job + mailer on `queue`. Call once at startup.
/// Re-calling replaces the previously registered mailer.
pub async fn register_email_job<Q: JobQueue>(queue: &Q, cfg: EmailJobConfig) {
    mailer_registry()
        .write()
        .expect("EmailJob registry poisoned")
        .insert(EmailJob::NAME, cfg.mailer);
    queue.register::<EmailJob>().await;
}

/// Enqueue an email for asynchronous delivery. Returns immediately;
/// delivery happens on a worker.
///
/// # Errors
/// Returns the underlying [`JobError::Queue`] when the enqueue fails
/// (DB unavailable, channel closed, payload not serializable).
pub async fn dispatch_email<Q: JobQueue>(queue: &Q, email: &Email) -> Result<(), JobError> {
    queue
        .dispatch(&EmailJob {
            email: email.clone(),
        })
        .await
}

/// Test-only — wipe the static mailer registry. Use between tests so
/// one fixture's mailer doesn't leak into the next.
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

    /// Serializes the tests in this module — they share the global
    /// mailer registry, so running in parallel would race.
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
        // Only register the Job (so dispatch works) — skip mailer.
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<EmailJob>().await;

        // Capture dead-letter so we know the worker rejected it.
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

        // The worker hits JobError::Queue immediately -> dead-letters
        // (Queue/Fatal are not retried).
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

        // No error, no observation — just doesn't panic / dead-letter.
        dispatch_email(&q, &email()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // pending_count should be back to 0 (job processed).
        assert_eq!(q.pending_count().await, 0);
        q.shutdown().await;
    }
}
