//! Django-parity #417 — file-based email backend.
//!
//! Verifies `FileMailer` writes `.eml` files containing the rendered
//! email headers + body, gives each send a unique filename, validates
//! input before touching the filesystem, and is selectable through
//! `email::from_settings` with `backend = "file"`.

#![cfg(all(feature = "email", feature = "config"))]

use std::sync::Arc;

use rustango::config::MailSettings;
use rustango::email::{from_settings, Email, FileMailer, MailError, Mailer};

fn unique_tmp_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rustango-file-mail-{label}-{pid}-{nanos}"))
}

#[tokio::test]
async fn file_mailer_writes_eml_with_headers_and_body() {
    let dir = unique_tmp_dir("basic");
    let mailer = FileMailer::new(&dir);
    let email = Email::new()
        .to("alice@example.com")
        .cc("audit@example.com")
        .from("noreply@example.com")
        .subject("Welcome")
        .body("Hello, Alice.");
    mailer.send(&email).await.expect("send ok");

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir exists")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(entries.len(), 1, "exactly one .eml written");
    let path = entries.pop().unwrap().path();
    assert_eq!(path.extension().and_then(|s| s.to_str()), Some("eml"));
    let body = std::fs::read_to_string(&path).expect("read .eml");

    assert!(body.contains("From: noreply@example.com"));
    assert!(body.contains("To: alice@example.com"));
    assert!(body.contains("Cc: audit@example.com"));
    assert!(body.contains("Subject: Welcome"));
    assert!(body.contains("Hello, Alice."));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn file_mailer_gives_each_send_unique_filename() {
    let dir = unique_tmp_dir("unique");
    let mailer = FileMailer::new(&dir);
    let make = |i: usize| {
        Email::new()
            .to(format!("recipient-{i}@example.com"))
            .from("noreply@example.com")
            .subject(format!("Msg {i}"))
            .body("body")
    };
    mailer.send(&make(1)).await.expect("send 1 ok");
    mailer.send(&make(2)).await.expect("send 2 ok");
    mailer.send(&make(3)).await.expect("send 3 ok");

    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir exists")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert_eq!(entries.len(), 3, "three unique files");
    let mut sorted = entries.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        3,
        "names should differ even within one second"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn file_mailer_rejects_invalid_email_without_writing() {
    let dir = unique_tmp_dir("invalid");
    let mailer = FileMailer::new(&dir);
    let err = mailer
        .send(&Email::new().subject("nobody-to").body("..."))
        .await
        .unwrap_err();
    assert!(matches!(err, MailError::InvalidMessage(_)));
    // The directory may have been created (auto-create runs after
    // validation in this impl), so check that no .eml landed.
    if dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir readable")
            .filter_map(Result::ok)
            .collect();
        assert!(entries.is_empty(), "no .eml on invalid send");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn from_settings_file_backend_writes_to_configured_dir() {
    let dir = unique_tmp_dir("settings");
    let s = MailSettings {
        backend: Some("file".into()),
        from_address: Some("noreply@example.com".into()),
        file_email_dir: Some(dir.clone()),
        ..Default::default()
    };
    let mailer: Arc<dyn Mailer> = from_settings(&s);
    let email = Email::new()
        .to("ops@example.com")
        .from("noreply@example.com")
        .subject("Hi")
        .body("body");
    mailer.send(&email).await.expect("send ok");

    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir exists")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(entries.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn from_settings_file_backend_without_dir_falls_back() {
    let s = MailSettings {
        backend: Some("file".into()),
        from_address: Some("noreply@example.com".into()),
        file_email_dir: None,
        ..Default::default()
    };
    // No panic, no error — just a warning + ConsoleMailer fallback.
    let mailer = from_settings(&s);
    let email = Email::new()
        .to("ops@example.com")
        .from("noreply@example.com")
        .subject("Hi")
        .body("body");
    mailer.send(&email).await.expect("fallback console send ok");
}
