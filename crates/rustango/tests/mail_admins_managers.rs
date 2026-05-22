//! Django-parity #416 — `mail_admins` / `mail_managers` helpers.
//!
//! Verifies the helper functions send to the right address list,
//! respect the empty-list no-op, and apply the right subject prefix.

#![cfg(all(feature = "email", feature = "config"))]

use rustango::config::MailSettings;
use rustango::email::{mail_admins, mail_managers, InMemoryMailer};

fn settings_with(admins: &[&str], managers: &[&str]) -> MailSettings {
    MailSettings {
        backend: Some("memory".into()),
        from_address: Some("noreply@example.com".into()),
        admins: admins.iter().map(|s| (*s).to_string()).collect(),
        managers: managers.iter().map(|s| (*s).to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn mail_admins_sends_to_each_admin() {
    let mailer = InMemoryMailer::new();
    let s = settings_with(&["alice@example.com", "bob@example.com"], &[]);
    let n = mail_admins(&mailer, &s, "Disk full", "/var is 100%")
        .await
        .unwrap();
    assert_eq!(n, 2);
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1, "one email sent (with 2 recipients)");
    assert_eq!(sent[0].to, vec!["alice@example.com", "bob@example.com"]);
    assert_eq!(sent[0].subject, "[admin] Disk full");
    assert_eq!(sent[0].body, "/var is 100%");
    assert_eq!(sent[0].from.as_deref(), Some("noreply@example.com"));
}

#[tokio::test]
async fn mail_admins_empty_list_is_a_silent_noop() {
    let mailer = InMemoryMailer::new();
    let s = settings_with(&[], &[]);
    let n = mail_admins(&mailer, &s, "x", "y").await.unwrap();
    assert_eq!(n, 0);
    assert!(mailer.sent().is_empty());
}

#[tokio::test]
async fn mail_managers_uses_manager_list_and_prefix() {
    let mailer = InMemoryMailer::new();
    let s = settings_with(&["dba@example.com"], &["ops-team@example.com"]);
    // Should hit only managers, not admins.
    let n = mail_managers(&mailer, &s, "Weekly summary", "all green")
        .await
        .unwrap();
    assert_eq!(n, 1);
    let sent = mailer.sent();
    assert_eq!(sent[0].to, vec!["ops-team@example.com"]);
    assert_eq!(sent[0].subject, "[manager] Weekly summary");
}

#[tokio::test]
async fn mail_admins_uses_admin_list_not_manager_list() {
    let mailer = InMemoryMailer::new();
    let s = settings_with(&["dba@example.com"], &["ops-team@example.com"]);
    mail_admins(&mailer, &s, "page", "msg").await.unwrap();
    let sent = mailer.sent();
    assert_eq!(sent[0].to, vec!["dba@example.com"]);
}
