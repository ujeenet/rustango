//! Backing test for `docs/email.md` — building an `Email`, sending through the
//! `Mailer` trait (captured by `InMemoryMailer` in tests), the `send_mail`
//! helper, validation, and CRLF header-injection defense.
//!
//! Run: `cargo test -p rustango --test email_doc`

#![cfg(feature = "email")]

use rustango::email::{send_mail, Email, InMemoryMailer, MailError};

#[tokio::test]
async fn email_sends_through_the_mailer_and_is_captured() {
    let mailer = InMemoryMailer::new(); // test backend: records instead of sending

    Email::new()
        .to("ada@example.com")
        .subject("Welcome")
        .body("Thanks for signing up.")
        .send(&mailer)
        .await
        .unwrap();

    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec!["ada@example.com".to_string()]);
    assert_eq!(sent[0].subject, "Welcome");
}

#[tokio::test]
async fn send_mail_helper_is_a_one_liner() {
    let mailer = InMemoryMailer::new();
    send_mail(
        &mailer,
        "Your report is ready",
        "Download it from your dashboard.",
        Some("noreply@example.com"),
        &["ops@example.com", "qa@example.com"],
    )
    .await
    .unwrap();

    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to.len(), 2);
    assert_eq!(sent[0].from.as_deref(), Some("noreply@example.com"));
}

#[test]
fn validate_rejects_incomplete_messages() {
    // No recipients.
    assert!(matches!(
        Email::new().subject("hi").validate(),
        Err(MailError::InvalidMessage(_))
    ));
    // Empty subject.
    assert!(matches!(
        Email::new().to("a@example.com").validate(),
        Err(MailError::InvalidMessage(_))
    ));
    // Complete message validates.
    assert!(Email::new()
        .to("a@example.com")
        .subject("ok")
        .validate()
        .is_ok());
}

#[test]
fn crlf_header_injection_is_rejected() {
    // A newline in the subject would let an attacker inject extra headers
    // (e.g. a hidden Bcc). Validation refuses it (Django's BadHeaderError).
    let sneaky = Email::new()
        .to("a@example.com")
        .subject("Hello\r\nBcc: victim@example.com")
        .body("x");
    assert!(matches!(sneaky.validate(), Err(MailError::BadHeader(_))));
}
