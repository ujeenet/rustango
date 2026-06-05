//! Django-parity tests for `Email::send(mailer)` builder-terminating
//! shape + `email::utils.formataddr`-style display-name formatter.

#![cfg(feature = "email")]

use rustango::email::{formataddr, parseaddr, Email, InMemoryMailer, Mailer, NullMailer};

// ------------------------------------------------------------------ Email::send

#[tokio::test]
async fn email_send_routes_to_supplied_mailer() {
    // Django parity: `EmailMessage(...).send()` posts the message to
    // the supplied connection. rustango spells the connection as a
    // `&dyn Mailer`.
    let m = InMemoryMailer::new();
    Email::new()
        .to("alice@example.com")
        .subject("hi")
        .body("hello")
        .send(&m)
        .await
        .unwrap();
    let sent = m.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec!["alice@example.com"]);
    assert_eq!(sent[0].subject, "hi");
    assert_eq!(sent[0].body, "hello");
}

#[tokio::test]
async fn email_send_surfaces_mailer_validation_error() {
    // Django parity: `send()` doesn't swallow validation errors —
    // missing recipients should surface as MailError::InvalidMessage,
    // not pass silently.
    let m = NullMailer;
    let err = Email::new().subject("x").body("y").send(&m).await;
    assert!(err.is_err(), "expected validation error on no-recipients");
}

#[tokio::test]
async fn email_send_keeps_existing_mailer_send_shape_working() {
    // Regression: `mailer.send(&email).await` (the pre-PR shape) must
    // still work — Email::send is additive, not a replacement.
    let m = InMemoryMailer::new();
    let e = Email::new().to("a@b.com").subject("s").body("b");
    m.send(&e).await.unwrap();
    assert_eq!(m.count(), 1);
}

// ------------------------------------------------------------------ formataddr

#[test]
fn formataddr_plain_name_passes_through_unquoted() {
    assert_eq!(
        formataddr(Some("Alice"), "alice@example.com"),
        "Alice <alice@example.com>",
    );
}

#[test]
fn formataddr_no_name_returns_raw_address() {
    assert_eq!(formataddr(None, "raw@example.com"), "raw@example.com");
}

#[test]
fn formataddr_empty_name_treated_as_no_name() {
    assert_eq!(formataddr(Some(""), "raw@example.com"), "raw@example.com");
    assert_eq!(
        formataddr(Some("   "), "raw@example.com"),
        "raw@example.com"
    );
}

#[test]
fn formataddr_quotes_name_with_comma() {
    // "Smith, John" — comma is an RFC 5322 special; must quote.
    assert_eq!(
        formataddr(Some("Smith, John"), "john@example.com"),
        r#""Smith, John" <john@example.com>"#,
    );
}

#[test]
fn formataddr_quotes_name_with_dot() {
    // Periods inside display name trip strict parsers; quote them.
    assert_eq!(
        formataddr(Some("Dr. Sarah"), "sarah@example.com"),
        r#""Dr. Sarah" <sarah@example.com>"#,
    );
}

#[test]
fn formataddr_escapes_embedded_double_quote() {
    // Internal `"` and `\` must be backslash-escaped per RFC 5322.
    assert_eq!(
        formataddr(Some(r#"O"Brien"#), "o@example.com"),
        r#""O\"Brien" <o@example.com>"#,
    );
}

#[test]
fn formataddr_escapes_embedded_backslash() {
    assert_eq!(
        formataddr(Some(r"path\name"), "x@example.com"),
        r#""path\\name" <x@example.com>"#,
    );
}

#[test]
fn formataddr_quotes_name_with_at_sign() {
    // `@` is reserved for the address; must quote when in display name.
    assert_eq!(
        formataddr(Some("alias@team"), "team@example.com"),
        r#""alias@team" <team@example.com>"#,
    );
}

#[test]
fn formataddr_round_trips_through_email_from() {
    // The whole point of formataddr is to drop into the From / To
    // header without further mangling. Verify the formatted string is
    // accepted as-is by Email::from.
    let formatted = formataddr(Some("Smith, John"), "john@example.com");
    let e = Email::new()
        .to("a@b.com")
        .from(&formatted)
        .subject("s")
        .body("b");
    assert_eq!(e.from.as_deref(), Some(formatted.as_str()));
}

// ------------------------------------------------------------------ parseaddr (Django parity inverse)

#[test]
fn parseaddr_splits_name_and_address() {
    let (name, addr) = parseaddr("Alice <alice@example.com>");
    assert_eq!(name, "Alice");
    assert_eq!(addr, "alice@example.com");
}

#[test]
fn parseaddr_bare_address_has_empty_name() {
    let (name, addr) = parseaddr("bare@example.com");
    assert!(name.is_empty());
    assert_eq!(addr, "bare@example.com");
}

#[test]
fn parseaddr_quoted_name_with_comma() {
    // Round-trip: formataddr puts the quotes, parseaddr strips them.
    let formatted = formataddr(Some("Smith, John"), "john@example.com");
    let (name, addr) = parseaddr(&formatted);
    assert_eq!(name, "Smith, John");
    assert_eq!(addr, "john@example.com");
}

#[test]
fn parseaddr_handles_escaped_quote_in_name() {
    // `"O\"Brien" <o@example.com>` → name = `O"Brien`
    let (name, addr) = parseaddr(r#""O\"Brien" <o@example.com>"#);
    assert_eq!(name, "O\"Brien");
    assert_eq!(addr, "o@example.com");
}

#[test]
fn parseaddr_handles_escaped_backslash() {
    let (name, addr) = parseaddr(r#""path\\name" <x@example.com>"#);
    assert_eq!(name, "path\\name");
    assert_eq!(addr, "x@example.com");
}

#[test]
fn parseaddr_empty_input() {
    let (name, addr) = parseaddr("");
    assert!(name.is_empty());
    assert!(addr.is_empty());
}

#[test]
fn parseaddr_trims_surrounding_whitespace() {
    let (name, addr) = parseaddr("  Alice <alice@example.com>  ");
    assert_eq!(name, "Alice");
    assert_eq!(addr, "alice@example.com");
}

#[test]
fn formataddr_parseaddr_round_trip() {
    // Every reasonable input survives the round-trip.
    for (name_opt, addr) in [
        (Some("Alice"), "alice@example.com"),
        (Some("Smith, John"), "j@b.com"),
        (None, "raw@b.com"),
        (Some("Dr. Sarah"), "sarah@example.com"),
    ] {
        let formatted = formataddr(name_opt, addr);
        let (got_name, got_addr) = parseaddr(&formatted);
        assert_eq!(
            got_addr, addr,
            "addr mismatch for input {name_opt:?} {addr:?}"
        );
        let expected_name = name_opt.unwrap_or("");
        assert_eq!(
            got_name, expected_name,
            "name mismatch for input {name_opt:?}"
        );
    }
}
