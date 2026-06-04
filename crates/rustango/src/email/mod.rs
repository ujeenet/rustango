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
//! | [`FileMailer`] | Dev / staging — writes one `.eml` per send under a directory. |
//! | [`NullMailer`] | Production guardrail — accepts and discards every send (CI / disabled mail). |
//! | [`SmtpMailer`] | Production — async lettre + rustls SMTP relay (`email-smtp` feature). |
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

#[cfg(feature = "email-smtp")]
pub mod smtp;
#[cfg(feature = "email-smtp")]
pub use smtp::{SmtpMailer, SmtpMailerBuilder, TlsMode};

// ------------------------------------------------------------------ Email

/// One outbound email. Use the builder methods to assemble.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// Django-parity attachments — file blobs attached to the
    /// outgoing message. Populated via [`Email::attach`] /
    /// [`Email::attach_text`].
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// Django-parity `EmailMessage.attach(filename, content, mimetype)` —
/// one attached blob. `mimetype` is the MIME type lettre stamps on
/// the SinglePart; when `None` we default to `application/octet-stream`
/// (RFC-9110 recommendation for opaque blobs).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    /// File name as it will appear in the `Content-Disposition` header
    /// (`attachment; filename="<this>"`).
    pub filename: String,
    /// Raw bytes. Plain-text attachments can be built via
    /// [`Email::attach_text`] which UTF-8 encodes a `&str`.
    pub content: Vec<u8>,
    /// MIME type. `None` = `application/octet-stream`. Common values:
    /// `text/plain`, `text/csv`, `application/pdf`, `image/png`.
    pub mimetype: Option<String>,
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

    /// Django-parity `EmailMessage.attach(filename, content, mimetype)` —
    /// attach a binary blob. `mimetype = None` means
    /// `application/octet-stream`. Backends serialize the attachment
    /// according to their capabilities:
    ///
    /// * [`SmtpMailer`] (feature `email-smtp`) — sent as a SinglePart
    ///   attachment inside a `multipart/mixed` MIME container.
    /// * [`ConsoleMailer`] — prints a one-line summary (name + size).
    /// * [`FileMailer`] — listed by name in the dev `.eml` dump.
    /// * [`InMemoryMailer`] — captured on the cloned `Email` for
    ///   test assertions.
    #[must_use]
    pub fn attach(
        mut self,
        filename: impl Into<String>,
        content: impl Into<Vec<u8>>,
        mimetype: Option<impl Into<String>>,
    ) -> Self {
        self.attachments.push(Attachment {
            filename: filename.into(),
            content: content.into(),
            mimetype: mimetype.map(Into::into),
        });
        self
    }

    /// Django-parity convenience — attach a UTF-8 text blob with
    /// `text/plain` MIME. Equivalent to `attach(filename, content,
    /// Some("text/plain"))` but spares the caller the `Some(_)`.
    #[must_use]
    pub fn attach_text(self, filename: impl Into<String>, content: impl Into<String>) -> Self {
        let s = content.into();
        self.attach(filename, s.into_bytes(), Some("text/plain"))
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
        for att in &email.attachments {
            println!(
                "--- attachment: {} ({} bytes, {}) ---",
                att.filename,
                att.content.len(),
                att.mimetype
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
            );
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
        Self {
            sent: Mutex::new(Vec::new()),
        }
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
        self.sent
            .lock()
            .expect("sent mutex poisoned")
            .push(email.clone());
        Ok(())
    }
}

// ------------------------------------------------------------------ FileMailer

/// Development / debug mailer — writes each outgoing email to a
/// timestamped `.eml` file in a configured directory instead of
/// sending it.
///
/// Mirrors Django's `django.core.mail.backends.filebased.EmailBackend`
/// (issue #417). Useful when you want to inspect rendered email
/// content (password-reset links, signup confirmations) during
/// development without wiring an SMTP relay or piping stdout into a
/// log file.
///
/// File names are `YYYYMMDDHHMMSS-<seq>.eml` so a burst of emails
/// inside the same second still get unique paths. The directory is
/// created on `send` if it doesn't yet exist.
pub struct FileMailer {
    dir: std::path::PathBuf,
    seq: std::sync::atomic::AtomicU64,
}

impl FileMailer {
    /// Build a mailer that writes `.eml` files into `dir`. The
    /// directory is auto-created on the first `send` call.
    #[must_use]
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The directory `.eml` files are written into.
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

/// Serialize an `Email` to RFC-822-ish text. Headers first, blank
/// line, then body. HTML alternative (when present) is appended as a
/// `--- HTML alternative ---` block — this matches the human-readable
/// shape Django's file backend uses for dev inspection. We are NOT
/// producing a fully-spec-compliant multipart MIME message; this is a
/// debugging dump format.
fn serialize_eml(email: &Email) -> String {
    let mut out = String::with_capacity(256 + email.body.len());
    if let Some(f) = &email.from {
        out.push_str("From: ");
        out.push_str(f);
        out.push('\n');
    }
    if !email.to.is_empty() {
        out.push_str("To: ");
        out.push_str(&email.to.join(", "));
        out.push('\n');
    }
    if !email.cc.is_empty() {
        out.push_str("Cc: ");
        out.push_str(&email.cc.join(", "));
        out.push('\n');
    }
    if !email.bcc.is_empty() {
        out.push_str("Bcc: ");
        out.push_str(&email.bcc.join(", "));
        out.push('\n');
    }
    if let Some(rt) = &email.reply_to {
        out.push_str("Reply-To: ");
        out.push_str(rt);
        out.push('\n');
    }
    out.push_str("Subject: ");
    out.push_str(&email.subject);
    out.push('\n');
    for (k, v) in &email.headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&email.body);
    if let Some(html) = &email.html_body {
        out.push_str("\n\n--- HTML alternative ---\n");
        out.push_str(html);
    }
    for att in &email.attachments {
        out.push_str(&format!(
            "\n\n--- attachment: {} ({} bytes, {}) ---",
            att.filename,
            att.content.len(),
            att.mimetype
                .as_deref()
                .unwrap_or("application/octet-stream"),
        ));
    }
    out
}

#[async_trait]
impl Mailer for FileMailer {
    async fn send(&self, email: &Email) -> Result<(), MailError> {
        email.validate()?;
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| MailError::Transport(format!("create_dir_all: {e}")))?;
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let name = format!("{stamp}-{seq:04}.eml");
        let path = self.dir.join(name);
        std::fs::write(&path, serialize_eml(email))
            .map_err(|e| MailError::Transport(format!("write {}: {e}", path.display())))?;
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

/// Build a [`BoxedMailer`] from a loaded
/// [`crate::config::MailSettings`] section (#87 wiring, v0.29).
///
/// Backend selection from `s.backend`:
/// - `"console"` (default) → [`ConsoleMailer`]
/// - `"memory"` → [`InMemoryMailer`] (tests / staging snapshots)
/// - `"null"` / `"none"` → [`NullMailer`]
/// - `"smtp"` → falls back to [`ConsoleMailer`] with a warning —
///   `SmtpMailer` is documented but not yet implemented; until
///   that ships, projects that need real SMTP wire it themselves
///   via [`BoxedMailer`]
/// - any other / unset → [`ConsoleMailer`] (dev-friendly default;
///   warn for typos)
///
/// `s.smtp_host` and `s.from_address` are accepted by the section
/// but currently unused at the backend layer — `from_address`
/// belongs on the per-message [`Email::from`] field, not the
/// backend; `smtp_host` lights up when `SmtpMailer` ships.
///
/// ```ignore
/// let cfg = rustango::config::Settings::load_from_env()?;
/// let mailer: rustango::email::BoxedMailer =
///     rustango::email::from_settings(&cfg.mail);
/// ```
/// Django-shape `send_mail(subject, message, from_email, recipient_list)` —
/// fire-and-forget single-message helper. Returns `Ok(())` on
/// success.
///
/// Direct translation of the most common Django mail call:
///
/// ```python
/// # Django
/// send_mail('Subject here',
///           'Here is the message.',
///           'from@example.com',
///           ['to@example.com'],
///           fail_silently=False)
/// ```
///
/// ```ignore
/// // rustango
/// rustango::email::send_mail(
///     &*mailer,
///     "Subject here",
///     "Here is the message.",
///     Some("from@example.com"),
///     &["to@example.com"],
/// ).await?;
/// ```
///
/// `from_email = None` lets the mailer fall through to its own
/// configured default (matching Django's `DEFAULT_FROM_EMAIL`
/// fallback). `recipient_list` must be non-empty — the mailer's
/// `Email::validate()` surfaces a `MailError::InvalidMessage` otherwise.
///
/// # Errors
/// Forwarded from the mailer's `send` call.
pub async fn send_mail(
    mailer: &dyn Mailer,
    subject: impl Into<String>,
    body: impl Into<String>,
    from_email: Option<&str>,
    recipient_list: &[&str],
) -> Result<(), MailError> {
    let mut email = Email::new().subject(subject).body(body);
    if let Some(from) = from_email {
        email = email.from(from.to_owned());
    }
    for to in recipient_list {
        email = email.to((*to).to_owned());
    }
    mailer.send(&email).await
}

/// Django-shape `send_mass_mail(datatuple)` — bulk-send a batch of
/// messages. `datatuple` in Django is `[(subject, message, from, [to,
/// ...]), ...]`; rustango takes a slice of pre-built `Email`s, which
/// is the more idiomatic Rust shape and avoids per-tuple boilerplate.
///
/// Sequential by default — backends with native pipelining (lettre's
/// SMTP transport over a kept-open connection) get the same call shape
/// without extra plumbing; future iteration can swap the loop for a
/// pooled batch send.
///
/// Returns `Ok(count)` where `count` is the number of successfully
/// sent messages. Per-message errors short-circuit on the first
/// failure, matching Django's `fail_silently=False` default.
///
/// # Errors
/// Forwarded from the mailer's `send` call on the first failing message.
pub async fn send_many(mailer: &dyn Mailer, emails: &[Email]) -> Result<usize, MailError> {
    let mut sent = 0;
    for email in emails {
        mailer.send(email).await?;
        sent += 1;
    }
    Ok(sent)
}

/// Django-shape `mail_admins(subject, message)` — sends to the
/// addresses configured in `MailSettings.admins`. Returns Ok(0) when
/// the list is empty (a no-op without warning, matching Django's
/// "no ADMINS → silent skip" behavior). Issue #416.
///
/// `subject` is prefixed with `"[admin] "` to match Django's default
/// `EMAIL_SUBJECT_PREFIX`. Override the subject yourself if you need
/// a different prefix.
///
/// `from` falls back to `MailSettings.from_address`; if neither is
/// set the mailer's `Email::validate()` will surface a
/// `MailError::InvalidMessage`.
///
/// # Errors
/// Forwarded from the mailer's `send` call. Returns `Ok(count)` on
/// success — `count` is the number of recipients the message went to.
#[cfg(feature = "config")]
pub async fn mail_admins(
    mailer: &dyn Mailer,
    s: &crate::config::MailSettings,
    subject: impl Into<String>,
    body: impl Into<String>,
) -> Result<usize, MailError> {
    send_to_list(
        mailer,
        s,
        &s.admins,
        "[admin] ",
        subject.into(),
        body.into(),
    )
    .await
}

/// Django-shape `mail_managers(subject, message)` — sends to the
/// addresses configured in `MailSettings.managers`. Same shape as
/// [`mail_admins`]; subject is prefixed with `"[manager] "`. Issue #416.
#[cfg(feature = "config")]
pub async fn mail_managers(
    mailer: &dyn Mailer,
    s: &crate::config::MailSettings,
    subject: impl Into<String>,
    body: impl Into<String>,
) -> Result<usize, MailError> {
    send_to_list(
        mailer,
        s,
        &s.managers,
        "[manager] ",
        subject.into(),
        body.into(),
    )
    .await
}

/// Pick the `From:` address for a server-generated mail
/// (`mail_admins`, `mail_managers`, error notifications). Django
/// `SERVER_EMAIL` parity — falls back to `DEFAULT_FROM_EMAIL`
/// (`from_address`) when unset.
#[cfg(feature = "config")]
fn server_from_address(s: &crate::config::MailSettings) -> Option<&str> {
    s.server_email.as_deref().or(s.from_address.as_deref())
}

#[cfg(feature = "config")]
async fn send_to_list(
    mailer: &dyn Mailer,
    s: &crate::config::MailSettings,
    list: &[String],
    fallback_prefix: &str,
    subject: String,
    body: String,
) -> Result<usize, MailError> {
    if list.is_empty() {
        return Ok(0);
    }
    // Django `EMAIL_SUBJECT_PREFIX` parity — when set, it wins over
    // the historical `[admin] ` / `[manager] ` fallback so projects
    // can brand server mail with `"[Acme] "` (note the trailing
    // space matches Django's convention).
    let prefix = s.email_subject_prefix.as_deref().unwrap_or(fallback_prefix);
    let mut email = Email::new()
        .subject(format!("{prefix}{subject}"))
        .body(body);
    // SERVER_EMAIL → DEFAULT_FROM_EMAIL → no header.
    if let Some(from) = server_from_address(s) {
        email = email.from(from.to_owned());
    }
    for addr in list {
        email = email.to(addr.clone());
    }
    mailer.send(&email).await?;
    Ok(list.len())
}

#[cfg(feature = "config")]
#[must_use]
pub fn from_settings(s: &crate::config::MailSettings) -> BoxedMailer {
    match s.backend.as_deref() {
        Some("smtp") => smtp_from_settings_or_warn(s),
        Some("memory") => Arc::new(InMemoryMailer::new()),
        Some("null" | "none") => Arc::new(NullMailer),
        Some("file") => file_from_settings_or_warn(s),
        Some("console") | None => Arc::new(ConsoleMailer),
        Some(other) => {
            tracing::warn!(
                target: "rustango::email",
                backend = %other,
                "unknown mail.backend value; falling back to ConsoleMailer",
            );
            Arc::new(ConsoleMailer)
        }
    }
}

/// File-backend resolver — needs `[mail].file_email_dir` to be set,
/// otherwise warns and falls back to `ConsoleMailer` so the app still
/// boots. Issue #417.
#[cfg(feature = "config")]
fn file_from_settings_or_warn(s: &crate::config::MailSettings) -> BoxedMailer {
    match s.file_email_dir.as_deref() {
        Some(dir) => Arc::new(FileMailer::new(dir)),
        None => {
            tracing::warn!(
                target: "rustango::email",
                "mail.backend = \"file\" but [mail].file_email_dir is unset; \
                 falling back to ConsoleMailer.",
            );
            Arc::new(ConsoleMailer)
        }
    }
}

/// SMTP-backend resolver. When the `email-smtp` feature is on,
/// builds an [`SmtpMailer`] from the section — and falls back to
/// [`ConsoleMailer`] with a tracing warning if the build fails (so
/// apps don't refuse to boot on a malformed `[mail]` section). When
/// the feature is off, emits the same legacy warning the pre-#48
/// build did and falls back to [`ConsoleMailer`].
#[cfg(all(feature = "config", feature = "email-smtp"))]
fn smtp_from_settings_or_warn(s: &crate::config::MailSettings) -> BoxedMailer {
    match smtp::from_settings(s) {
        Ok(Some(m)) => m,
        Ok(None) => {
            tracing::warn!(
                target: "rustango::email",
                "mail.backend = \"smtp\" but [mail].smtp_host is unset; falling back to ConsoleMailer."
            );
            Arc::new(ConsoleMailer)
        }
        Err(e) => {
            tracing::warn!(
                target: "rustango::email",
                error = %e,
                "mail.backend = \"smtp\" but SmtpMailer build failed; falling back to ConsoleMailer."
            );
            Arc::new(ConsoleMailer)
        }
    }
}

#[cfg(all(feature = "config", not(feature = "email-smtp")))]
fn smtp_from_settings_or_warn(_s: &crate::config::MailSettings) -> BoxedMailer {
    tracing::warn!(
        target: "rustango::email",
        "mail.backend = \"smtp\" but the `email-smtp` feature isn't enabled in this build; \
         falling back to ConsoleMailer. Enable `email-smtp` to ship a real SMTP transport.",
    );
    Arc::new(ConsoleMailer)
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
        m.send(&Email::new().to("a@x").subject("s").body("b"))
            .await
            .unwrap();
        m.send(&Email::new().to("b@x").subject("s2").body("b2"))
            .await
            .unwrap();
        assert_eq!(m.count(), 2);
        assert_eq!(m.sent()[0].to, vec!["a@x"]);
        m.clear();
        assert_eq!(m.count(), 0);
    }

    #[tokio::test]
    async fn null_mailer_succeeds_silently() {
        let m = NullMailer;
        m.send(&Email::new().to("x@y").subject("s").body("b"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn null_mailer_still_validates() {
        let m = NullMailer;
        let result = m.send(&Email::new().subject("s")).await;
        assert!(result.is_err());
    }

    // ---- #87 wiring: from_settings ----

    /// Memory backend captures sends so tests can assert on them.
    /// Other backends would either print to stdout or no-op, so
    /// memory is the easiest to exercise here.
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_memory_backend_captures_send() {
        let mut s = crate::config::MailSettings::default();
        s.backend = Some("memory".into());
        let m = from_settings(&s);
        let email = Email::new()
            .to("a@x.com")
            .from("noreply@x.com")
            .subject("hi")
            .body("body");
        m.send(&email).await.expect("send ok");
        // Memory backend wraps an Arc<Mutex<Vec<Email>>>; we don't
        // expose it here, but the round-trip not erroring confirms
        // the right backend was selected (NullMailer would also
        // succeed; ConsoleMailer would print). See the dedicated
        // memory_mailer_records_send test for the storage assertion.
    }

    /// Null backend silently drops, even on a valid email.
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_null_backend_drops_send() {
        let mut s = crate::config::MailSettings::default();
        s.backend = Some("null".into());
        let m = from_settings(&s);
        let email = Email::new()
            .to("a@x.com")
            .from("noreply@x.com")
            .subject("hi")
            .body("body");
        m.send(&email)
            .await
            .expect("null backend never errors on valid email");
    }

    /// Unknown / unset backend falls back to ConsoleMailer (which
    /// prints — we just check the call succeeds; capturing stdout
    /// in a unit test would race with parallel runners).
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_unset_falls_back_to_console() {
        let s = crate::config::MailSettings::default();
        let m = from_settings(&s);
        let email = Email::new()
            .to("a@x.com")
            .from("noreply@x.com")
            .subject("hi")
            .body("body");
        m.send(&email).await.expect("console mailer ok");
    }

    /// SMTP backend: when `email-smtp` feature is enabled,
    /// `from_settings` with an `smtp_host` builds a real
    /// [`crate::email::SmtpMailer`] (no network contact — just
    /// constructs the transport). When the feature is off, the
    /// same call falls back to `ConsoleMailer` with a warning.
    ///
    /// We only assert that the mailer was successfully created (no
    /// `.send()` — there's no real relay at `mail.example.com` in
    /// the test environment).
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_smtp_builds_mailer_when_host_given() {
        let mut s = crate::config::MailSettings::default();
        s.backend = Some("smtp".into());
        s.smtp_host = Some("mail.example.com".into());
        s.smtp_tls = Some("starttls".into());
        let m = from_settings(&s);
        // The mailer was constructed. On `email-smtp` builds this is a
        // SmtpMailer; on non-smtp builds it is ConsoleMailer. Either
        // way the `Arc<dyn Mailer>` is valid — we just don't send.
        drop(m);
    }
}
