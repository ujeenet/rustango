//! Django-parity `EmailMessage.attach(filename, content, mimetype)` —
//! attachment builder + backend serialization.

#![cfg(feature = "email")]

use rustango::email::{Attachment, ConsoleMailer, Email, FileMailer, InMemoryMailer, Mailer};

fn base_email() -> Email {
    Email::new()
        .to("alice@example.com")
        .from("noreply@example.com")
        .subject("hi")
        .body("greetings")
}

// ------------------------------------------------------------------ builder

#[test]
fn attach_appends_one_attachment() {
    let e = base_email().attach("report.csv", b"a,b\n1,2".to_vec(), Some("text/csv"));
    assert_eq!(e.attachments.len(), 1);
    let a: &Attachment = &e.attachments[0];
    assert_eq!(a.filename, "report.csv");
    assert_eq!(a.content, b"a,b\n1,2");
    assert_eq!(a.mimetype.as_deref(), Some("text/csv"));
}

#[test]
fn attach_with_none_mimetype_stores_none() {
    // None → backends substitute application/octet-stream at send-time,
    // but the struct preserves the original None for round-tripping.
    let e = base_email().attach("blob.bin", vec![0u8, 1, 2, 3], None::<String>);
    assert!(e.attachments[0].mimetype.is_none());
}

#[test]
fn attach_can_be_chained_for_multiple_files() {
    let e = base_email()
        .attach("a.txt", b"a".to_vec(), Some("text/plain"))
        .attach("b.pdf", vec![0xDE, 0xAD], Some("application/pdf"));
    assert_eq!(e.attachments.len(), 2);
    assert_eq!(e.attachments[0].filename, "a.txt");
    assert_eq!(e.attachments[1].filename, "b.pdf");
}

#[test]
fn attach_text_helper_uses_text_plain() {
    let e = base_email().attach_text("notes.txt", "hello\nworld");
    assert_eq!(e.attachments.len(), 1);
    let a = &e.attachments[0];
    assert_eq!(a.filename, "notes.txt");
    assert_eq!(a.content, b"hello\nworld");
    assert_eq!(a.mimetype.as_deref(), Some("text/plain"));
}

#[test]
fn default_email_has_zero_attachments() {
    // Sanity: existing call sites that never touch .attach still get
    // an empty vec — no behavior change for unattached mail.
    let e = base_email();
    assert!(e.attachments.is_empty());
}

// ------------------------------------------------------------------ InMemoryMailer roundtrip

#[tokio::test]
async fn in_memory_mailer_captures_attachments() {
    let m = InMemoryMailer::new();
    let e = base_email()
        .attach("a.txt", b"hi".to_vec(), Some("text/plain"))
        .attach("b.bin", vec![1, 2, 3], None::<String>);
    m.send(&e).await.unwrap();
    let sent = m.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].attachments.len(), 2);
    assert_eq!(sent[0].attachments[0].filename, "a.txt");
    assert_eq!(sent[0].attachments[1].content, vec![1, 2, 3]);
    assert!(sent[0].attachments[1].mimetype.is_none());
}

// ------------------------------------------------------------------ ConsoleMailer

#[tokio::test]
async fn console_mailer_accepts_email_with_attachments() {
    // ConsoleMailer prints to stdout — we can't assert on output here
    // (stdout is process-wide), so this is a smoke test that the
    // render path doesn't panic / error when attachments are present.
    let m = ConsoleMailer;
    let e = base_email().attach_text("note.txt", "captured");
    m.send(&e).await.unwrap();
}

// ------------------------------------------------------------------ FileMailer .eml dump

#[tokio::test]
async fn file_mailer_lists_attachments_in_eml_dump() {
    let dir = std::env::temp_dir().join(format!("rustango-email-attach-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let m = FileMailer::new(&dir);
    let e = base_email()
        .attach("data.csv", b"x,y\n1,2".to_vec(), Some("text/csv"))
        .attach("photo.png", vec![0x89, 0x50, 0x4E, 0x47], Some("image/png"));
    m.send(&e).await.unwrap();
    // Find the single .eml file just dropped.
    let eml: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|d| {
            d.path()
                .extension()
                .and_then(|s| s.to_str())
                .map_or(false, |s| s == "eml")
        })
        .collect();
    assert_eq!(eml.len(), 1, ".eml file present");
    let text = std::fs::read_to_string(eml[0].path()).unwrap();
    assert!(text.contains("attachment: data.csv"));
    assert!(text.contains("text/csv"));
    assert!(text.contains("attachment: photo.png"));
    assert!(text.contains("image/png"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ unattached round-trip preserved

#[tokio::test]
async fn unattached_email_still_serializes_cleanly() {
    // Regression guard — pre-PR call sites that build an Email and
    // never call .attach should produce identical-looking dumps to
    // before this PR.
    let dir = std::env::temp_dir().join(format!("rustango-email-noattach-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let m = FileMailer::new(&dir);
    m.send(&base_email()).await.unwrap();
    let eml: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    let text = std::fs::read_to_string(eml[0].path()).unwrap();
    assert!(!text.contains("attachment:"));
    let _ = std::fs::remove_dir_all(&dir);
}
