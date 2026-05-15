//! Live integration test for `SmtpMailer` (issue #48). Runs a
//! one-shot mock SMTP server on a random local port, hands the
//! mailer the address, sends an email, and asserts that the bytes
//! the server received parse back into the right `From:` / `To:` /
//! `Subject:` / body.
//!
//! Plain TCP (no TLS) so the mock can be ~150 lines of tokio rather
//! than a fully-fledged TLS terminator — the TLS modes are exercised
//! at the unit-test level via `TlsMode::default_port()` etc.

#![cfg(feature = "email-smtp")]

use std::time::Duration;

use rustango::email::smtp::{SmtpMailer, TlsMode};
use rustango::email::{Email, Mailer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// One-shot mock SMTP server. Listens on a random local port,
/// accepts a single client, runs the smallest RFC-2821 dialogue
/// (EHLO → MAIL FROM → RCPT TO → DATA → QUIT), captures the DATA
/// body, and sends it back through the oneshot.
///
/// Returns `(port, body_rx)`. Spawn before connecting the mailer.
async fn spawn_mock() -> (u16, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);

        // 220 banner.
        write.write_all(b"220 mock.test ESMTP\r\n").await.unwrap();

        let mut body = String::new();
        let mut in_data = false;

        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap_or(0);
            if n == 0 {
                break;
            }

            if in_data {
                if line == ".\r\n" {
                    in_data = false;
                    write.write_all(b"250 Ok: queued\r\n").await.unwrap();
                    continue;
                }
                body.push_str(&line);
                continue;
            }

            let upper = line.to_ascii_uppercase();
            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                // Single-line EHLO so lettre doesn't think we
                // advertise STARTTLS / AUTH.
                write.write_all(b"250 mock.test\r\n").await.unwrap();
            } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
                write.write_all(b"250 Ok\r\n").await.unwrap();
            } else if upper.starts_with("DATA") {
                write
                    .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                    .await
                    .unwrap();
                in_data = true;
            } else if upper.starts_with("QUIT") {
                write.write_all(b"221 Bye\r\n").await.unwrap();
                break;
            } else if upper.starts_with("NOOP") || upper.starts_with("RSET") {
                write.write_all(b"250 Ok\r\n").await.unwrap();
            } else {
                // Be lenient — accept anything else as 250 Ok so the
                // mock doesn't have to know every verb lettre might
                // emit at handshake time.
                write.write_all(b"250 Ok\r\n").await.unwrap();
            }
        }

        let _ = tx.send(body);
    });

    (port, rx)
}

#[tokio::test]
async fn smtp_mailer_delivers_envelope_and_body() {
    let (port, body_rx) = spawn_mock().await;

    let mailer = SmtpMailer::builder("127.0.0.1")
        .port(port)
        .tls(TlsMode::None)
        .build()
        .expect("build ok");

    let email = Email::new()
        .from("noreply@example.com")
        .to("alice@example.com")
        .subject("hello from rustango")
        .body("test body line one\r\ntest body line two");

    // Don't let the test hang forever if the mock wedges.
    tokio::time::timeout(Duration::from_secs(5), mailer.send(&email))
        .await
        .expect("send within timeout")
        .expect("send ok");

    let body = tokio::time::timeout(Duration::from_secs(5), body_rx)
        .await
        .expect("body received within timeout")
        .expect("oneshot delivered");

    assert!(
        body.contains("Subject: hello from rustango"),
        "Subject header missing in DATA: {body}"
    );
    assert!(
        body.contains("From: noreply@example.com"),
        "From header missing in DATA: {body}"
    );
    assert!(
        body.contains("To: alice@example.com"),
        "To header missing in DATA: {body}"
    );
    assert!(
        body.contains("test body line one"),
        "body line 1 missing: {body}"
    );
    assert!(
        body.contains("test body line two"),
        "body line 2 missing: {body}"
    );
}

#[tokio::test]
async fn smtp_mailer_uses_default_from_when_email_omits_from() {
    let (port, body_rx) = spawn_mock().await;

    let mailer = SmtpMailer::builder("127.0.0.1")
        .port(port)
        .tls(TlsMode::None)
        .default_from("fallback@example.com")
        .build()
        .expect("build ok");

    let email = Email::new()
        .to("bob@example.com")
        .subject("default-from test")
        .body("body");

    tokio::time::timeout(Duration::from_secs(5), mailer.send(&email))
        .await
        .expect("send within timeout")
        .expect("send ok");

    let body = tokio::time::timeout(Duration::from_secs(5), body_rx)
        .await
        .expect("body received")
        .expect("oneshot delivered");

    assert!(
        body.contains("From: fallback@example.com"),
        "default_from didn't apply: {body}"
    );
}

#[tokio::test]
async fn smtp_mailer_errors_when_no_from_and_no_default() {
    let mailer = SmtpMailer::builder("127.0.0.1")
        .port(2525) // any port — we won't connect
        .tls(TlsMode::None)
        .build()
        .expect("build ok");

    let email = Email::new()
        .to("c@example.com")
        .subject("no from")
        .body("body");

    let r = mailer.send(&email).await;
    assert!(r.is_err(), "should reject missing from");
    let msg = format!("{}", r.unwrap_err());
    assert!(
        msg.contains("`from`") && msg.contains("default_from"),
        "error should mention missing from: {msg}"
    );
}

#[tokio::test]
async fn smtp_mailer_sends_html_alternative() {
    let (port, body_rx) = spawn_mock().await;

    let mailer = SmtpMailer::builder("127.0.0.1")
        .port(port)
        .tls(TlsMode::None)
        .build()
        .expect("build ok");

    let email = Email::new()
        .from("noreply@example.com")
        .to("alice@example.com")
        .subject("html test")
        .body("plain text part")
        .html_body("<p>html part</p>");

    tokio::time::timeout(Duration::from_secs(5), mailer.send(&email))
        .await
        .expect("send within timeout")
        .expect("send ok");

    let body = tokio::time::timeout(Duration::from_secs(5), body_rx)
        .await
        .expect("body received")
        .expect("oneshot");

    assert!(
        body.to_lowercase().contains("multipart/alternative"),
        "expected multipart/alternative content-type: {body}"
    );
    assert!(body.contains("plain text part"), "plaintext part missing");
    assert!(body.contains("<p>html part</p>"), "html part missing");
}
