//! Email backend layer — pluggable async email sending.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::email::{Mailer, Email, ConsoleMailer};
//! use std::sync::Arc;
//!
//! let mailer: Arc<dyn Mailer> = Arc::new(ConsoleMailer::default());
//!
//! let email = Email::new()
//!     .to("user@example.com")
//!     .from("noreply@my-app.com")
//!     .subject("Welcome!")
//!     .body("Thanks for signing up.");
//! mailer.send(&email).await?;
//! ```
//!
//! ## Backends
//!
//! | Backend | When to use |
//! |---------|-------------|
//! | [`ConsoleMailer`] | Development — prints emails to stdout. Default. |
//! | [`InMemoryMailer`] | Tests — captures emails into a `Vec` for assertions. |
//! | [`SmtpMailer`] | Production — connects to an SMTP relay. (Future slice — currently a stub.) |
//!
//! ## Plug your own
//!
//! Implement `Mailer` for any third-party transport (SES, SendGrid, Postmark):
//!
//! ```ignore
//! use rustango::email::{Mailer, Email, MailError};
//! use async_trait::async_trait;
//!
//! pub struct SesMailer { /* ... */ }
//!
//! #[async_trait]
//! impl Mailer for SesMailer {
//!     async fn send(&self, email: &Email) -> Result<(), MailError> {
//!         // POST to AWS SES, etc.
//!         Ok(())
//!     }
//! }
//! ```

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

// ------------------------------------------------------------------ Email

/// One outbound email. Use the builder methods to assemble.
#[derive(Debug, Clone, Default)]
pub struct Email {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub from: Option<String>,
    pub reply_to: Option<String>,
    pub subject: String,
    pub body: String,
    pub html_body: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl Email {
    /// Construct an empty email — chain builder methods to fill in fields.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a `To` recipient. Call multiple times for multiple recipients.
    #[must_use]
    pub fn to(mut self, addr: impl Into<String>) -> Self {
        self.to.push(addr.into());
        self
    }

    /// Add a `Cc` recipient.
    #[must_use]
    pub fn cc(mut self, addr: impl Into<String>) -> Self {
        self.cc.push(addr.into());
        self
    }

    /// Add a `Bcc` recipient.
    #[must_use]
    pub fn bcc(mut self, addr: impl Into<String>) -> Self {
        self.bcc.push(addr.into());
        self
    }

    /// Set the `From` address.
    #[must_use]
    pub fn from(mut self, addr: impl Into<String>) -> Self {
        self.from = Some(addr.into());
        self
    }

    /// Set the `Reply-To` address.
    #[must_use]
    pub fn reply_to(mut self, addr: impl Into<String>) -> Self {
        self.reply_to = Some(addr.into());
        self
    }

    /// Set the subject line.
    #[must_use]
    pub fn subject(mut self, s: impl Into<String>) -> Self {
        self.subject = s.into();
        self
    }

    /// Set the plaintext body.
    #[must_use]
    pub fn body(mut self, b: impl Into<String>) -> Self {
        self.body = b.into();
        self
    }

    /// Set an HTML alternative body. Sent as a multipart/alternative MIME
    /// when both `body` and `html_body` are present.
    #[must_use]
    pub fn html_body(mut self, b: impl Into<String>) -> Self {
        self.html_body = Some(b.into());
        self
    }

    /// Add a custom header.
    #[must_use]
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Validate the minimum required fields: at least one recipient + non-empty subject.
    pub fn validate(&self) -> Result<(), MailError> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err(MailError::InvalidMessage("no recipients".into()));
        }
        if self.subject.is_empty() {
            return Err(MailError::InvalidMessage("subject is empty".into()));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ MailError

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("invalid message: {0}")]
    InvalidMessage(String),
    #[error("transport error: {0}")]
    Transport(String),
}

// ------------------------------------------------------------------ Mailer trait

/// Pluggable async email backend.
#[async_trait]
pub trait Mailer: Send + Sync + 'static {
    /// Send `email`. Implementations should validate before transmitting
    /// (the helper [`Email::validate`] is the canonical check).
    async fn send(&self, email: &Email) -> Result<(), MailError>;
}

/// `Arc<dyn Mailer>` alias.
pub type BoxedMailer = Arc<dyn Mailer>;

// ------------------------------------------------------------------ ConsoleMailer

/// Development mailer — prints emails to stdout instead of sending.
#[derive(Default)]
pub struct ConsoleMailer;

#[async_trait]
impl Mailer for ConsoleMailer {
    async fn send(&self, email: &Email) -> Result<(), MailError> {
        email.validate()?;
        println!("============= [ConsoleMailer] outgoing =============");
        if let Some(f) = &email.from {
            println!("From: {f}");
        }
        if !email.to.is_empty() {
            println!("To: {}", email.to.join(", "));
        }
        if !email.cc.is_empty() {
            println!("Cc: {}", email.cc.join(", "));
        }
        if !email.bcc.is_empty() {
            println!("Bcc: {}", email.bcc.join(", "));
        }
        if let Some(rt) = &email.reply_to {
            println!("Reply-To: {rt}");
        }
        println!("Subject: {}", email.subject);
        for (k, v) in &email.headers {
            println!("{k}: {v}");
        }
        println!();
        println!("{}", email.body);
        if let Some(html) = &email.html_body {
            println!("\n--- HTML alternative ---\n{html}");
        }
        println!("====================================================");
        Ok(())
    }
}

// ------------------------------------------------------------------ InMemoryMailer

/// Test mailer — captures every sent email into a shared `Vec` for assertions.
#[derive(Default)]
pub struct InMemoryMailer {
    sent: Mutex<Vec<Email>>,
}

impl InMemoryMailer {
    #[must_use]
    pub fn new() -> Self {
        Self { sent: Mutex::new(Vec::new()) }
    }

    /// Snapshot all emails sent so far. Doesn't clear the buffer.
    #[must_use]
    pub fn sent(&self) -> Vec<Email> {
        self.sent.lock().expect("sent mutex poisoned").clone()
    }

    /// Number of emails sent so far.
    #[must_use]
    pub fn count(&self) -> usize {
        self.sent.lock().expect("sent mutex poisoned").len()
    }

    /// Clear the captured email buffer.
    pub fn clear(&self) {
        self.sent.lock().expect("sent mutex poisoned").clear();
    }
}

#[async_trait]
impl Mailer for InMemoryMailer {
    async fn send(&self, email: &Email) -> Result<(), MailError> {
        email.validate()?;
        self.sent.lock().expect("sent mutex poisoned").push(email.clone());
        Ok(())
    }
}

// ------------------------------------------------------------------ NullMailer

/// Discards all emails. Useful for disabling email sending in environments
/// (e.g. CI) without changing call sites.
#[derive(Default)]
pub struct NullMailer;

#[async_trait]
impl Mailer for NullMailer {
    async fn send(&self, email: &Email) -> Result<(), MailError> {
        email.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn email_builder_chains() {
        let e = Email::new()
            .to("a@x.com")
            .to("b@x.com")
            .from("noreply@my.app")
            .subject("hi")
            .body("hello");
        assert_eq!(e.to, vec!["a@x.com", "b@x.com"]);
        assert_eq!(e.from.as_deref(), Some("noreply@my.app"));
        assert_eq!(e.subject, "hi");
    }

    #[tokio::test]
    async fn validate_rejects_no_recipients() {
        let e = Email::new().subject("x").body("y");
        assert!(matches!(e.validate(), Err(MailError::InvalidMessage(_))));
    }

    #[tokio::test]
    async fn validate_rejects_empty_subject() {
        let e = Email::new().to("x@y.com").body("z");
        assert!(matches!(e.validate(), Err(MailError::InvalidMessage(_))));
    }

    #[tokio::test]
    async fn in_memory_mailer_captures_sent() {
        let m = InMemoryMailer::new();
        m.send(&Email::new().to("a@x").subject("s").body("b")).await.unwrap();
        m.send(&Email::new().to("b@x").subject("s2").body("b2")).await.unwrap();
        assert_eq!(m.count(), 2);
        assert_eq!(m.sent()[0].to, vec!["a@x"]);
        m.clear();
        assert_eq!(m.count(), 0);
    }

    #[tokio::test]
    async fn null_mailer_succeeds_silently() {
        let m = NullMailer;
        m.send(&Email::new().to("x@y").subject("s").body("b")).await.unwrap();
    }

    #[tokio::test]
    async fn null_mailer_still_validates() {
        let m = NullMailer;
        let result = m.send(&Email::new().subject("s")).await;
        assert!(result.is_err());
    }
}
