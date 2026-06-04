//! Django-parity `send_mail` + `send_many` (= `send_mass_mail`) helpers.

#![cfg(feature = "email")]

use rustango::email::{send_mail, send_many, Email, InMemoryMailer};

// ------------------------------------------------------------------ send_mail

#[tokio::test]
async fn send_mail_dispatches_one_message() {
    let m = InMemoryMailer::new();
    send_mail(
        &m,
        "hello",
        "world",
        Some("noreply@example.com"),
        &["alice@example.com", "bob@example.com"],
    )
    .await
    .unwrap();
    let sent = m.sent();
    assert_eq!(sent.len(), 1, "single message goes out");
    assert_eq!(sent[0].subject, "hello");
    assert_eq!(sent[0].body, "world");
    assert_eq!(sent[0].from.as_deref(), Some("noreply@example.com"));
    assert_eq!(sent[0].to, vec!["alice@example.com", "bob@example.com"],);
}

#[tokio::test]
async fn send_mail_with_none_from_omits_from_field() {
    // Django parity: from_email=None defers to the mailer's default.
    let m = InMemoryMailer::new();
    send_mail(&m, "s", "b", None, &["x@example.com"])
        .await
        .unwrap();
    let sent = m.sent();
    assert!(
        sent[0].from.is_none(),
        "no `from` should be set when callsite passes None"
    );
}

#[tokio::test]
async fn send_mail_empty_recipient_list_errors() {
    // Validate() rejects no-recipients — surfaces as MailError.
    let m = InMemoryMailer::new();
    let err = send_mail(&m, "s", "b", Some("f@x.com"), &[]).await;
    assert!(err.is_err(), "expected invalid-message error");
}

// ------------------------------------------------------------------ send_many

#[tokio::test]
async fn send_many_sends_each_email_in_order() {
    let m = InMemoryMailer::new();
    let emails = vec![
        Email::new().to("a@example.com").subject("first").body("1"),
        Email::new().to("b@example.com").subject("second").body("2"),
        Email::new().to("c@example.com").subject("third").body("3"),
    ];
    let count = send_many(&m, &emails).await.unwrap();
    assert_eq!(count, 3);
    let sent = m.sent();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0].subject, "first");
    assert_eq!(sent[1].subject, "second");
    assert_eq!(sent[2].subject, "third");
}

#[tokio::test]
async fn send_many_empty_slice_is_zero_send() {
    let m = InMemoryMailer::new();
    let count = send_many(&m, &[]).await.unwrap();
    assert_eq!(count, 0);
    assert!(m.sent().is_empty());
}

#[tokio::test]
async fn send_many_short_circuits_on_first_invalid_message() {
    // Matches Django's `fail_silently=False` default — the first bad
    // message halts the batch. Sent count == valid messages that made
    // it through BEFORE the bad one.
    let m = InMemoryMailer::new();
    let emails = vec![
        Email::new().to("ok@example.com").subject("a").body("x"),
        // No recipient → validate() rejects.
        Email::new().subject("bad").body("x"),
        Email::new().to("never@example.com").subject("c").body("x"),
    ];
    let err = send_many(&m, &emails).await;
    assert!(err.is_err(), "expected error on second message");
    // First made it through; third should not have been attempted.
    let sent = m.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].subject, "a");
}
