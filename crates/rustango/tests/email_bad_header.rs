//! Django-parity `BadHeaderError` — reject CR / LF in single-line
//! header fields to defend against email header injection attacks.
//! Django raises the same on `EmailMessage(subject='x\nBcc: evil@a')`.

#![cfg(feature = "email")]

use rustango::email::{Email, MailError};

fn base_valid() -> Email {
    Email::new()
        .to("alice@example.com")
        .subject("hello")
        .body("greetings")
}

#[test]
fn validate_accepts_clean_email() {
    assert!(base_valid().validate().is_ok());
}

// ------------------------------------------------------------------ subject

#[test]
fn newline_in_subject_rejected_as_bad_header() {
    let e = Email::new()
        .to("alice@example.com")
        .subject("hi\nBcc: evil@attacker.com")
        .body("...");
    let err = e.validate().unwrap_err();
    assert!(
        matches!(err, MailError::BadHeader(_)),
        "expected BadHeader, got {err:?}",
    );
}

#[test]
fn carriage_return_in_subject_rejected() {
    let e = Email::new()
        .to("a@b.com")
        .subject("hi\rBcc: evil@a")
        .body("body");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

#[test]
fn crlf_pair_in_subject_rejected() {
    let e = Email::new()
        .to("a@b.com")
        .subject("hi\r\nBcc: evil@a")
        .body("body");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

// ------------------------------------------------------------------ from / reply_to

#[test]
fn newline_in_from_rejected() {
    let e = base_valid().from("noreply@x.com\nBcc: evil@a");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

#[test]
fn newline_in_reply_to_rejected() {
    let e = base_valid().reply_to("ops@x.com\nBcc: evil@a");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

// ------------------------------------------------------------------ recipients

#[test]
fn newline_in_to_rejected() {
    let e = Email::new()
        .to("alice@example.com\nBcc: evil@a")
        .subject("s")
        .body("b");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

#[test]
fn newline_in_cc_rejected() {
    let e = Email::new()
        .to("alice@example.com")
        .cc("ops@example.com\nBcc: evil@a")
        .subject("s")
        .body("b");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

#[test]
fn newline_in_bcc_rejected() {
    let e = Email::new()
        .to("alice@example.com")
        .bcc("audit@example.com\nBcc: leaks-everywhere")
        .subject("s")
        .body("b");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

// ------------------------------------------------------------------ headers

#[test]
fn newline_in_custom_header_name_rejected() {
    let e = base_valid().header("X-Foo\nBcc: evil@a", "value");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

#[test]
fn newline_in_custom_header_value_rejected() {
    let e = base_valid().header("X-Foo", "v\r\nBcc: evil@a");
    assert!(matches!(e.validate(), Err(MailError::BadHeader(_))));
}

// ------------------------------------------------------------------ body OK

#[test]
fn newline_in_body_remains_allowed() {
    // The body is multi-line by design — only header fields enforce
    // the no-CRLF rule. Verify we didn't accidentally over-validate.
    let e = Email::new()
        .to("a@b.com")
        .subject("s")
        .body("line one\nline two\nline three");
    assert!(e.validate().is_ok());
}
