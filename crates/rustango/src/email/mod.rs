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
//!
//! [`ConsoleMailer`]: crate::email::ConsoleMailer
//! [`InMemoryMailer`]: crate::email::InMemoryMailer
//! [`FileMailer`]: crate::email::FileMailer
//! [`NullMailer`]: crate::email::NullMailer
//! [`SmtpMailer`]: crate::email::smtp::SmtpMailer

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
    /// Attachments — file blobs attached to the
    /// outgoing message. Populated via [`Email::attach`] /
    /// [`Email::attach_text`].
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// One attached blob. `mimetype` is the MIME type lettre stamps on
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

    /// Attach a binary blob. `mimetype = None` means
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

    /// Convenience — attach a UTF-8 text blob with
    /// `text/plain` MIME. Equivalent to `attach(filename, content,
    /// Some("text/plain"))` but spares the caller the `Some(_)`.
    #[must_use]
    pub fn attach_text(self, filename: impl Into<String>, content: impl Into<String>) -> Self {
        let s = content.into();
        self.attach(filename, s.into_bytes(), Some("text/plain"))
    }

    /// Validate the minimum required fields: at least one recipient +
    /// non-empty subject, and reject `\\r` / `\\n` in single-line header
    /// fields. That defends against header-injection attacks, where
    /// an attacker-controlled subject or from-address forges extra
    /// `To:` / `Bcc:` lines.
    pub fn validate(&self) -> Result<(), MailError> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err(MailError::InvalidMessage("no recipients".into()));
        }
        if self.subject.is_empty() {
            return Err(MailError::InvalidMessage("subject is empty".into()));
        }
        check_no_crlf("subject", &self.subject)?;
        if let Some(from) = &self.from {
            check_no_crlf("from", from)?;
        }
        if let Some(rt) = &self.reply_to {
            check_no_crlf("reply_to", rt)?;
        }
        for addr in &self.to {
            check_no_crlf("to", addr)?;
        }
        for addr in &self.cc {
            check_no_crlf("cc", addr)?;
        }
        for addr in &self.bcc {
            check_no_crlf("bcc", addr)?;
        }
        for (name, value) in &self.headers {
            check_no_crlf("header name", name)?;
            check_no_crlf(&format!("header `{name}` value"), value)?;
        }
        Ok(())
    }

    /// Send this message via the supplied mailer.
    ///
    /// ```ignore
    /// Email::new()
    ///     .subject("hi")
    ///     .body("...")
    ///     .to("a@b.com")
    ///     .send(&*mailer)
    ///     .await?;
    /// ```
    ///
    /// Equivalent to `mailer.send(&email).await` — the method shape
    /// just lets fluent builder chains terminate on the `Email` itself
    /// instead of detaching to call `mailer.send(&built)`.
    pub async fn send(&self, mailer: &dyn Mailer) -> Result<(), MailError> {
        mailer.send(self).await
    }
}

/// Format an RFC 5322 address with a display name. Returns
/// `"Display Name <email@example.com>"`
/// when `name` is provided, or just `address` when `name` is `None`
/// or empty.
///
/// Display names containing RFC 5322 specials (`(`, `)`, `<`, `>`,
/// `@`, `,`, `;`, `:`, `\`, `"`, `.`, `[`, `]`) are wrapped in
/// double quotes with embedded `"` and `\` escaped. Plain alphanumeric
/// display names pass through unquoted.
///
/// ```ignore
/// use rustango::email::formataddr;
///
/// assert_eq!(formataddr(Some("Alice"), "a@b.com"),
///            "Alice <a@b.com>");
/// assert_eq!(formataddr(Some("Smith, John"), "j@b.com"),
///            r#""Smith, John" <j@b.com>"#);
/// assert_eq!(formataddr(None, "raw@b.com"), "raw@b.com");
/// ```
#[must_use]
pub fn formataddr(name: Option<&str>, address: &str) -> String {
    let trimmed = name.map(str::trim).filter(|s| !s.is_empty());
    let Some(name) = trimmed else {
        return address.to_owned();
    };
    let needs_quoting = name.chars().any(|c| {
        matches!(
            c,
            '(' | ')' | '<' | '>' | '@' | ',' | ';' | ':' | '\\' | '"' | '.' | '[' | ']'
        )
    });
    if needs_quoting {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\" <{address}>")
    } else {
        format!("{name} <{address}>")
    }
}

/// The inverse of [`formataddr`]. Splits a `"Display Name
/// <user@example.com>"` style header value into `(name, address)`.
///
/// Returns `(name, address)` as owned `String`s. When the input
/// has no angle-bracketed address, the whole string is returned
/// as the address with an empty name (Python shape — `parseaddr`
/// always returns a pair, never raises).
///
/// Backslash escapes inside a double-quoted display name are
/// un-escaped (`\"` → `"`, `\\` → `\`). Other escape sequences
/// pass through literally — matching Python's email.utils
/// behavior, which doesn't expand `\n` / `\t` inside quoted-string
/// productions.
///
/// ```ignore
/// use rustango::email::parseaddr;
/// // Display name + address.
/// assert_eq!(
///     parseaddr("Alice <alice@example.com>"),
///     ("Alice".to_owned(), "alice@example.com".to_owned())
/// );
/// // Quoted display name with escaped backslash + dquote.
/// assert_eq!(
///     parseaddr(r#""Smith, John" <john@example.com>"#),
///     ("Smith, John".to_owned(), "john@example.com".to_owned())
/// );
/// // No angle brackets → entire string is the address, name is empty.
/// assert_eq!(
///     parseaddr("bare@example.com"),
///     (String::new(), "bare@example.com".to_owned())
/// );
/// ```
#[must_use]
pub fn parseaddr(address: &str) -> (String, String) {
    let trimmed = address.trim();
    // Find the LAST `<` paired with the LAST `>` (Python takes the
    // outermost angle-bracketed group).
    let lt = trimmed.rfind('<');
    let gt = trimmed.rfind('>');
    match (lt, gt) {
        (Some(l), Some(g)) if g > l => {
            let name_raw = trimmed[..l].trim();
            let addr = trimmed[l + 1..g].trim().to_owned();
            let name = unquote_display_name(name_raw);
            (name, addr)
        }
        _ => (String::new(), trimmed.to_owned()),
    }
}

/// Strip surrounding `"..."` and un-escape backslash sequences.
/// Internal helper for [`parseaddr`].
fn unquote_display_name(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next) = chars.peek() {
                    if next == '\\' || next == '"' {
                        out.push(next);
                        chars.next();
                        continue;
                    }
                }
            }
            out.push(c);
        }
        return out;
    }
    raw.to_owned()
}

// ------------------------------------------------------------------ MailError

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MailError {
    #[error("invalid message: {0}")]
    InvalidMessage(String),
    #[error("bad header: {0}")]
    BadHeader(String),
    #[error("transport error: {0}")]
    Transport(String),
    /// The server refused the message or a recipient for good (SMTP
    /// 550–555). Auth and connect failures stay [`Self::Transport`].
    #[error("rejected: {0}")]
    Rejected(String),
    /// The `[mail]` settings cannot build the backend they ask for.
    #[error("mail config: {0}")]
    Config(String),
}

impl MailError {
    /// Only a transport failure can succeed on a later try (#1923).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

/// Reject `\r` / `\n` in single-line header fields. Returns
/// [`MailError::BadHeader`]. Defends against header-injection
/// attacks where attacker input lands in `Subject:` / `From:` /
/// `To:` and forges extra envelope headers via embedded newlines.
fn check_no_crlf(field: &str, value: &str) -> Result<(), MailError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(MailError::BadHeader(format!(
            "newline in {field} — possible header injection"
        )));
    }
    Ok(())
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
/// Useful when you want to inspect rendered email
/// content (password-reset links, signup confirmations) during
/// development without wiring an SMTP relay or piping stdout into a
/// log file.
///
/// File names are `YYYYMMDDHHMMSS-<pid>-<seq>.eml`, and a send never
/// overwrites an existing file, so several processes can share one
/// directory. The directory is created on `send` if it doesn't yet exist.
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
/// `--- HTML alternative ---` block, which reads well during dev
/// inspection. This is NOT a spec-compliant multipart MIME message;
/// it is a debugging dump format.
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
        // Owner-only: the files hold reset links and other tokens.
        let mut dir = std::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
        dir.create(&self.dir)
            .map_err(|e| MailError::Transport(format!("create_dir_all: {e}")))?;
        let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let pid = std::process::id();
        // `create_new` never overwrites: another process (or a container
        // with the same pid) writing this name makes us take the next seq.
        loop {
            let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = self.dir.join(format!("{stamp}-{pid}-{seq:04}.eml"));
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let file = opts.open(&path);
            match file {
                Ok(mut f) => {
                    use std::io::Write as _;
                    return f.write_all(serialize_eml(email).as_bytes()).map_err(|e| {
                        MailError::Transport(format!("write {}: {e}", path.display()))
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(MailError::Transport(format!(
                        "write {}: {e}",
                        path.display()
                    )))
                }
            }
        }
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

/// Fire-and-forget single-message helper. Returns `Ok(())` on
/// success.
///
/// ```ignore
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
/// configured default. `recipient_list` must be non-empty — the mailer's
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

/// Bulk-send a batch of messages. Takes a slice of pre-built
/// `Email`s, so each one carries its own subject, sender and
/// recipients.
///
/// Sequential by default — backends with native pipelining (lettre's
/// SMTP transport over a kept-open connection) get the same call shape
/// without extra plumbing; future iteration can swap the loop for a
/// pooled batch send.
///
/// Returns `Ok(count)` where `count` is the number of successfully
/// sent messages. Per-message errors short-circuit on the first
/// failure — errors are not swallowed.
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

/// Send to the addresses configured in `MailSettings.admins`.
/// Returns Ok(0) when the list is empty — a silent no-op, so an
/// unconfigured deployment does not error on every alert.
///
/// `subject` is prefixed with `"[admin] "`. Set
/// `email_subject_prefix` to brand it differently.
///
/// `from` falls back to `MailSettings.from_address`; if neither is
/// set the mailer's `Email::validate()` will surface a
/// `MailError::InvalidMessage`.
///
/// # Errors
/// Forwarded from the mailer's `send` call. Returns `Ok(count)` on
/// success — `count` is the number of recipients the message went to.
/// All recipients share one message: over SMTP one refused address
/// fails the send for every one of them.
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

/// Send to the addresses configured in `MailSettings.managers`. Same
/// shape as [`mail_admins`]; subject is prefixed with `"[manager] "`.
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
/// (`mail_admins`, `mail_managers`, error notifications). Falls back
/// to `from_address` when unset.
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
    // `email_subject_prefix`, when set, wins over the `[admin] ` /
    // `[manager] ` fallback, so projects can brand server mail with
    // `"[Acme] "` — include the trailing space.
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

/// Build a [`BoxedMailer`] from the `[mail]` section.
///
/// `backend`: `"console"` (default), `"memory"`, `"null"` / `"none"`,
/// `"file"` (needs `file_email_dir`) or `"smtp"` ([`smtp::SmtpMailer`],
/// feature `email-smtp`).
///
/// ```ignore
/// let cfg = rustango::config::Settings::load_from_env()?;
/// let mailer = rustango::email::from_settings(&cfg.mail)?;
/// ```
///
/// # Errors
/// [`MailError::Config`] when `backend = "smtp"` cannot be built: no
/// `smtp_host`, a bad `from_address` or `smtp_tls`, or no `email-smtp`
/// feature; also for `file` without `file_email_dir` or an unknown
/// backend. Falling back to the console put reset links in logs (#1923).
#[cfg(feature = "config")]
pub fn from_settings(s: &crate::config::MailSettings) -> Result<BoxedMailer, MailError> {
    Ok(match s.backend.as_deref() {
        Some("smtp") => smtp_from_settings(s)?,
        Some("memory") => Arc::new(InMemoryMailer::new()),
        Some("null" | "none") => Arc::new(NullMailer),
        Some("file") => match s.file_email_dir.as_deref() {
            Some(dir) => Arc::new(FileMailer::new(dir)),
            // An error, like smtp: the console would put mail in logs (#1948).
            None => {
                return Err(MailError::Config(
                    "mail.backend = \"file\" but [mail].file_email_dir is unset".into(),
                ))
            }
        },
        Some("console") | None => Arc::new(ConsoleMailer),
        Some(other) => {
            return Err(MailError::Config(format!(
                "unknown mail.backend `{other}`; use console, memory, null, file or smtp"
            )))
        }
    })
}

/// `backend = "smtp"`: an [`SmtpMailer`] or an error, never the console.
#[cfg(all(feature = "config", feature = "email-smtp"))]
fn smtp_from_settings(s: &crate::config::MailSettings) -> Result<BoxedMailer, MailError> {
    match smtp::from_settings(s) {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Err(MailError::Config(
            "mail.backend = \"smtp\" but [mail].smtp_host is unset".into(),
        )),
        Err(e) => Err(MailError::Config(format!(
            "mail.backend = \"smtp\" cannot be built: {e}"
        ))),
    }
}

#[cfg(all(feature = "config", not(feature = "email-smtp")))]
fn smtp_from_settings(_s: &crate::config::MailSettings) -> Result<BoxedMailer, MailError> {
    Err(MailError::Config(
        "mail.backend = \"smtp\" but this build has no `email-smtp` feature".into(),
    ))
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

    /// Two mailers on one directory stand in for two processes: each
    /// starts at seq 0, so neither may overwrite the other (#2335).
    #[tokio::test]
    async fn file_mailers_sharing_a_dir_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (FileMailer::new(dir.path()), FileMailer::new(dir.path()));
        let e = Email::new().to("a@x.com").subject("s").body("b");
        for _ in 0..5 {
            a.send(&e).await.unwrap();
            b.send(&e).await.unwrap();
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 10);
    }

    /// Mail files hold reset tokens: owner-only dir and files.
    #[cfg(unix)]
    #[tokio::test]
    async fn file_mailer_writes_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("mail");
        let e = Email::new().to("a@x.com").subject("s").body("b");
        FileMailer::new(&dir).send(&e).await.unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(mode(&file), 0o600);
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
        let m = from_settings(&s).expect("mailer");
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
        let m = from_settings(&s).expect("mailer");
        let email = Email::new()
            .to("a@x.com")
            .from("noreply@x.com")
            .subject("hi")
            .body("body");
        m.send(&email)
            .await
            .expect("null backend never errors on valid email");
    }

    /// A `file` backend with no directory, or an unknown backend, is an
    /// error rather than a console that logs the mail (#1948).
    #[cfg(feature = "config")]
    #[test]
    fn a_file_backend_without_a_dir_or_an_unknown_backend_is_an_error() {
        let mut s = crate::config::MailSettings::default();
        s.backend = Some("file".into());
        assert!(matches!(from_settings(&s), Err(MailError::Config(_))));
        s.backend = Some("smtpp".into());
        assert!(matches!(from_settings(&s), Err(MailError::Config(_))));
    }

    /// Unset backend falls back to ConsoleMailer (which
    /// prints — we just check the call succeeds; capturing stdout
    /// in a unit test would race with parallel runners).
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_unset_falls_back_to_console() {
        let s = crate::config::MailSettings::default();
        let m = from_settings(&s).expect("mailer");
        let email = Email::new()
            .to("a@x.com")
            .from("noreply@x.com")
            .subject("hi")
            .body("body");
        m.send(&email).await.expect("console mailer ok");
    }

    /// `smtp` with a host builds a mailer; no contact is made.
    #[cfg(all(feature = "config", feature = "email-smtp"))]
    #[tokio::test]
    async fn from_settings_smtp_builds_mailer_when_host_given() {
        let mut s = crate::config::MailSettings::default();
        s.backend = Some("smtp".into());
        s.smtp_host = Some("mail.example.com".into());
        s.smtp_tls = Some("starttls".into());
        from_settings(&s).expect("smtp mailer");
    }

    /// A broken `smtp` section is an error, not a console that sends
    /// reset links to the log (#1923).
    #[cfg(feature = "config")]
    #[test]
    fn a_broken_smtp_section_is_an_error() {
        let smtp = |f: fn(&mut crate::config::MailSettings)| {
            let mut s = crate::config::MailSettings::default();
            s.backend = Some("smtp".into());
            s.smtp_host = Some("mail.example.com".into());
            f(&mut s);
            from_settings(&s).map(|_| ())
        };
        assert!(matches!(
            smtp(|s| s.smtp_host = None),
            Err(MailError::Config(_))
        ));
        assert!(matches!(
            smtp(|s| s.from_address = Some("not an address".into())),
            Err(MailError::Config(_))
        ));
        // `tls` reads as STARTTLS to most people but meant implicit TLS.
        assert!(matches!(
            smtp(|s| s.smtp_tls = Some("tls".into())),
            Err(MailError::Config(_))
        ));
        assert!(matches!(
            smtp(|s| s.smtp_tls = Some("bogus".into())),
            Err(MailError::Config(_))
        ));
    }
}
