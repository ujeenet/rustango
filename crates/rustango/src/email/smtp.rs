//! Production SMTP mailer — issue #48.
//!
//! Gated behind the `email-smtp` feature so default builds don't pull
//! in `lettre` + `rustls` + the rest of the SMTP transport stack.
//! Connects to an external relay (Postmark, SendGrid, AWS SES via
//! SMTP, etc.) and sends MIME-assembled mail.
//!
//! ## TLS modes
//!
//! | `tls_mode` | Wire shape | Typical port |
//! |---|---|---|
//! | [`TlsMode::None`] | Plain TCP — RFC-2821 SMTP. No encryption. | 25 |
//! | [`TlsMode::StartTls`] | Plain → upgrade via `STARTTLS` (RFC 3207). **Default.** | 587 |
//! | [`TlsMode::Implicit`] | TLS-from-byte-zero (SMTPS, RFC 8314 §3.3). | 465 |
//!
//! `rustls` only — no openssl. The TLS root store is `webpki-roots`
//! (the same CAs Mozilla ships); set up custom certificates by
//! building a `lettre::transport::smtp::client::Tls` manually and
//! passing through [`SmtpMailer::with_transport`].

use std::sync::Arc;

use async_trait::async_trait;
use lettre::message::{
    header::ContentType, Attachment as LettreAttachment, Mailbox, Message, MultiPart, SinglePart,
};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::Tls;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use super::{Email, MailError, Mailer};

/// TLS posture for the SMTP connection. Defaults to [`TlsMode::StartTls`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsMode {
    /// Plain TCP, no encryption. Only safe on localhost / internal
    /// networks. Servers conventionally listen on port 25.
    None,
    /// Plain TCP then upgrade via the `STARTTLS` verb. Default.
    /// Servers conventionally listen on port 587.
    #[default]
    StartTls,
    /// TLS-from-byte-zero (SMTPS / "implicit TLS"). Servers
    /// conventionally listen on port 465.
    Implicit,
}

impl TlsMode {
    /// Parse the string form used in [`crate::config::MailSettings::smtp_tls`].
    /// Unknown values map to [`TlsMode::StartTls`] (the safe default).
    #[must_use]
    pub fn from_str_loose(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "none" | "plain" | "off" => Self::None,
            "implicit" | "smtps" | "tls" => Self::Implicit,
            "starttls" | "" => Self::StartTls,
            _ => {
                tracing::warn!(
                    target: "rustango::email::smtp",
                    given = %s,
                    "unknown smtp.tls value; falling back to starttls"
                );
                Self::StartTls
            }
        }
    }

    /// Conventional SMTP port for this TLS mode. Used when no port
    /// is explicitly configured.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::None => 25,
            Self::StartTls => 587,
            Self::Implicit => 465,
        }
    }
}

/// Production SMTP mailer.
///
/// ```ignore
/// use rustango::email::smtp::{SmtpMailer, TlsMode};
///
/// let mailer = SmtpMailer::builder("mail.example.com")
///     .port(587)
///     .credentials("postmaster@example.com", "secret")
///     .tls(TlsMode::StartTls)
///     .build()?;
/// mailer.send(&email).await?;
/// ```
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    /// Pre-validated default `From:` mailbox, used when an outbound
    /// [`Email`] omits `from`. `None` requires every email to carry
    /// its own from-address (matches the existing `Mailer` shape).
    default_from: Option<Mailbox>,
}

impl SmtpMailer {
    /// Start a [`SmtpMailerBuilder`] for the relay at `host`.
    /// Defaults: port 587, [`TlsMode::StartTls`], no auth, no
    /// default-from. Chain `.port()` / `.credentials()` / `.tls()` /
    /// `.default_from()` to override, then `.build()`.
    #[must_use]
    pub fn builder(host: impl Into<String>) -> SmtpMailerBuilder {
        SmtpMailerBuilder {
            host: host.into(),
            port: None,
            credentials: None,
            tls: TlsMode::default(),
            default_from: None,
        }
    }

    /// Construct from a pre-built `lettre` transport. Use this when
    /// you need custom TLS roots, a non-default timeout, etc. that
    /// the builder doesn't expose.
    #[must_use]
    pub fn with_transport(transport: AsyncSmtpTransport<Tokio1Executor>) -> Self {
        Self {
            transport,
            default_from: None,
        }
    }

    /// Set the default `From:` mailbox to fall back to when an
    /// outbound [`Email`] omits `from`.
    ///
    /// # Errors
    /// Returns [`MailError::InvalidMessage`] if `from` doesn't parse
    /// as an RFC-5322 mailbox.
    pub fn default_from(mut self, from: &str) -> Result<Self, MailError> {
        self.default_from = Some(parse_mailbox(from)?);
        Ok(self)
    }
}

/// Builder for [`SmtpMailer`].
#[must_use = "call .build() to construct the SmtpMailer"]
pub struct SmtpMailerBuilder {
    host: String,
    port: Option<u16>,
    credentials: Option<(String, String)>,
    tls: TlsMode,
    default_from: Option<String>,
}

impl SmtpMailerBuilder {
    /// Override the TCP port. Default is derived from the TLS mode
    /// (25 / 587 / 465).
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Enable SMTP AUTH (PLAIN / LOGIN — `lettre` picks the
    /// strongest mechanism the server advertises). Both fields must
    /// be set; calling this method enables auth, omitting it leaves
    /// the transport anonymous.
    pub fn credentials(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials = Some((username.into(), password.into()));
        self
    }

    /// Override the TLS mode. Default is [`TlsMode::StartTls`].
    pub fn tls(mut self, mode: TlsMode) -> Self {
        self.tls = mode;
        self
    }

    /// Set the default `From:` address. Optional — when unset, every
    /// outbound [`Email`] must carry its own `from` field.
    pub fn default_from(mut self, from: impl Into<String>) -> Self {
        self.default_from = Some(from.into());
        self
    }

    /// Finalize. Validates `default_from` (if set) and assembles the
    /// `lettre` transport.
    ///
    /// # Errors
    /// Returns [`MailError::InvalidMessage`] for unparseable
    /// `default_from`; [`MailError::Transport`] when `lettre` rejects
    /// the host string (typically only on invalid hostnames).
    pub fn build(self) -> Result<SmtpMailer, MailError> {
        let port = self.port.unwrap_or_else(|| self.tls.default_port());

        // Choose the right lettre constructor for the TLS mode.
        let mut builder = match self.tls {
            TlsMode::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host),
            TlsMode::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
                .map_err(|e| {
                MailError::Transport(format!("starttls_relay({}): {e}", self.host))
            })?,
            TlsMode::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)
                .map_err(|e| MailError::Transport(format!("relay({}): {e}", self.host)))?,
        }
        .port(port);

        if let Some((user, pass)) = self.credentials {
            builder = builder.credentials(Credentials::new(user, pass));
        }

        // For TlsMode::None, lettre's `builder_dangerous` already
        // disables TLS — leave it as-is. For starttls / implicit,
        // the chosen constructor already wired the right TLS slot.
        if matches!(self.tls, TlsMode::None) {
            builder = builder.tls(Tls::None);
        }

        let transport = builder.build();
        let default_from = match self.default_from {
            Some(addr) => Some(parse_mailbox(&addr)?),
            None => None,
        };
        Ok(SmtpMailer {
            transport,
            default_from,
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, email: &Email) -> Result<(), MailError> {
        email.validate()?;

        let from = match email.from.as_deref() {
            Some(addr) => parse_mailbox(addr)?,
            None => self.default_from.clone().ok_or_else(|| {
                MailError::InvalidMessage(
                    "Email has no `from`, and SmtpMailer has no default_from".into(),
                )
            })?,
        };

        let mut builder = Message::builder().from(from);
        for to in &email.to {
            builder = builder.to(parse_mailbox(to)?);
        }
        for cc in &email.cc {
            builder = builder.cc(parse_mailbox(cc)?);
        }
        for bcc in &email.bcc {
            builder = builder.bcc(parse_mailbox(bcc)?);
        }
        if let Some(rt) = &email.reply_to {
            builder = builder.reply_to(parse_mailbox(rt)?);
        }
        builder = builder.subject(&email.subject);

        // Custom headers go on the Message via the builder's header
        // method; lettre validates them via the typed-header machinery,
        // so unknown header names just become "Header" structs.
        // We use the loose `header::Header` form via `headers_mut`.
        // Body part: text-only, or multipart/alternative when HTML present.
        let body_part = if let Some(html) = &email.html_body {
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(email.body.clone()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(html.clone()),
                )
        } else {
            MultiPart::mixed().singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(email.body.clone()),
            )
        };

        // Attachments — wrap the body part in `multipart/mixed` plus one
        // SinglePart per attachment when any are present. Skips the
        // extra wrapper when the list is empty (preserves v1 layout).
        let body_part = if email.attachments.is_empty() {
            body_part
        } else {
            let mut mixed = MultiPart::mixed().multipart(body_part);
            for att in &email.attachments {
                let ctype: ContentType = att
                    .mimetype
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(ContentType::parse("application/octet-stream").unwrap());
                mixed = mixed.singlepart(
                    LettreAttachment::new(att.filename.clone()).body(att.content.clone(), ctype),
                );
            }
            mixed
        };

        // NB: custom `email.headers` aren't forwarded in v1 — lettre's
        // raw-header API is fiddly (requires a typed `Header` impl per
        // name) and the existing in-process mailers don't actually
        // wire them anywhere meaningful either. Filed as a follow-up:
        // user code that needs `X-Mailgun-Tag` / `List-Unsubscribe`
        // can drop to the lettre builder directly via
        // `SmtpMailer::with_transport` until a clean abstraction lands.
        if !email.headers.is_empty() {
            tracing::warn!(
                target: "rustango::email::smtp",
                count = email.headers.len(),
                "Email.headers ignored — SmtpMailer v1 only forwards the standard envelope; \
                 custom headers will land in a follow-up slice."
            );
        }

        let message = builder
            .multipart(body_part)
            .map_err(|e| MailError::InvalidMessage(format!("message build: {e}")))?;

        self.transport
            .send(message)
            .await
            .map_err(|e| MailError::Transport(format!("smtp send: {e}")))?;
        Ok(())
    }
}

fn parse_mailbox(addr: &str) -> Result<Mailbox, MailError> {
    addr.parse::<Mailbox>()
        .map_err(|e| MailError::InvalidMessage(format!("bad address `{addr}`: {e}")))
}

/// Build an [`SmtpMailer`] (as a [`super::BoxedMailer`]) from the
/// loaded [`crate::config::MailSettings`]. Returns `None` when the
/// section doesn't carry an `smtp_host` — caller falls back to the
/// generic `from_settings` path.
///
/// # Errors
/// Returns [`MailError`] when the host / port / TLS / credentials /
/// default-from combination fails to build a transport.
#[cfg(feature = "config")]
pub fn from_settings(
    s: &crate::config::MailSettings,
) -> Result<Option<super::BoxedMailer>, MailError> {
    let Some(host) = s.smtp_host.as_deref() else {
        return Ok(None);
    };
    let tls = s
        .smtp_tls
        .as_deref()
        .map_or(TlsMode::default(), TlsMode::from_str_loose);
    let mut b = SmtpMailer::builder(host).tls(tls);
    if let Some(port) = s.smtp_port {
        b = b.port(port);
    }
    if let (Some(u), Some(p)) = (s.smtp_username.as_deref(), s.smtp_password.as_deref()) {
        b = b.credentials(u, p);
    }
    if let Some(addr) = s.from_address.as_deref() {
        b = b.default_from(addr);
    }
    Ok(Some(Arc::new(b.build()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_mode_from_str_handles_synonyms() {
        assert_eq!(TlsMode::from_str_loose("starttls"), TlsMode::StartTls);
        assert_eq!(TlsMode::from_str_loose("STARTTLS"), TlsMode::StartTls);
        assert_eq!(TlsMode::from_str_loose("none"), TlsMode::None);
        assert_eq!(TlsMode::from_str_loose("off"), TlsMode::None);
        assert_eq!(TlsMode::from_str_loose("plain"), TlsMode::None);
        assert_eq!(TlsMode::from_str_loose("implicit"), TlsMode::Implicit);
        assert_eq!(TlsMode::from_str_loose("smtps"), TlsMode::Implicit);
        assert_eq!(TlsMode::from_str_loose(""), TlsMode::StartTls);
        // Unknown → fallback.
        assert_eq!(TlsMode::from_str_loose("nope"), TlsMode::StartTls);
    }

    #[test]
    fn tls_mode_default_port_matches_conventions() {
        assert_eq!(TlsMode::None.default_port(), 25);
        assert_eq!(TlsMode::StartTls.default_port(), 587);
        assert_eq!(TlsMode::Implicit.default_port(), 465);
    }

    #[test]
    fn builder_sets_default_from() {
        let mailer = SmtpMailer::builder("localhost")
            .port(2525)
            .tls(TlsMode::None)
            .default_from("noreply@example.com")
            .build()
            .expect("build ok");
        assert!(mailer.default_from.is_some());
    }

    #[test]
    fn builder_rejects_unparseable_default_from() {
        let r = SmtpMailer::builder("localhost")
            .tls(TlsMode::None)
            .default_from("not a real address")
            .build();
        assert!(matches!(r, Err(MailError::InvalidMessage(_))));
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_returns_none_without_host() {
        let s = crate::config::MailSettings::default();
        let r = from_settings(&s).expect("ok");
        assert!(r.is_none(), "no smtp_host → None, caller falls back");
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_builds_with_host_and_creds() {
        let mut s = crate::config::MailSettings::default();
        s.smtp_host = Some("localhost".into());
        s.smtp_port = Some(2525);
        s.smtp_tls = Some("none".into());
        s.smtp_username = Some("user".into());
        s.smtp_password = Some("pass".into());
        s.from_address = Some("noreply@example.com".into());
        let m = from_settings(&s).expect("ok").expect("some");
        // Smoke: the mailer exists. Real SMTP round-trip is exercised
        // by the integration test against the mock server.
        drop(m);
    }
}
