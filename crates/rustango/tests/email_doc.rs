//! Backing test for `docs/email.md` — building an `Email`, sending through the
//! `Mailer` trait (captured by `InMemoryMailer` in tests), the `send_mail`
//! helper, validation, and CRLF header-injection defense. The handler,
//! template and queued-sending examples compile here, and the `[mail]` block
//! is loaded from the page.
//!
//! Run: `cargo test -p rustango --test email_doc`

#![cfg(feature = "email")]

#[path = "support/doc_snippet.rs"]
mod doc_snippet;

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
    // (e.g. a hidden Bcc). Validation refuses it.
    let sneaky = Email::new()
        .to("a@example.com")
        .subject("Hello\r\nBcc: victim@example.com")
        .body("x");
    assert!(matches!(sneaky.validate(), Err(MailError::BadHeader(_))));
}

// ------------------------------------------------- the page's app examples
//
// Each marked block below is a code example of `docs/email.md`, copied
// verbatim; `every_locale_publishes_the_tested_snippets` keeps them in sync.
// `myblog` in the `main.rs` examples is the module the example's route is in.

#[test]
fn every_locale_publishes_the_tested_snippets() {
    doc_snippet::assert_published(include_str!("email_doc.rs"), "email.md");
}

/// `src/contact.rs` from "Send from a handler".
#[rustfmt::skip]
mod sync_app {
    pub mod contact {
        // doc-snippet:start
        // src/contact.rs
        use axum::{http::StatusCode, routing::post, Extension, Json, Router};
        use rustango::email::{BoxedMailer, Email};
        use serde::Deserialize;

        #[derive(Deserialize)]
        pub struct ContactForm {
            pub email: String,
            pub message: String,
        }

        async fn contact(
            Extension(mailer): Extension<BoxedMailer>,
            Json(form): Json<ContactForm>,
        ) -> StatusCode {
            let sent = Email::new()
                .to("support@example.com")
                .reply_to(form.email)
                .subject("Contact form")
                .body(form.message)
                .send(&*mailer)
                .await;
            match sent {
                Ok(()) => StatusCode::NO_CONTENT,
                // The mail server is down or timed out: the user can try again.
                Err(e) if e.is_retryable() => StatusCode::SERVICE_UNAVAILABLE,
                // A bad address or a newline in a header.
                Err(_) => StatusCode::BAD_REQUEST,
            }
        }

        pub fn router(mailer: BoxedMailer) -> Router {
            Router::new()
                .route("/contact", post(contact))
                .layer(Extension(mailer))
        }
        // doc-snippet:end
    }
}

/// `src/contact.rs` from "Send it off the request".
#[cfg(feature = "jobs")]
#[rustfmt::skip]
mod queued_app {
    pub mod contact {
        // doc-snippet:start
        // src/contact.rs
        use std::sync::Arc;

        use axum::{http::StatusCode, routing::post, Extension, Json, Router};
        use rustango::email::Email;
        use rustango::email_jobs::dispatch_email;
        use rustango::jobs::{InMemoryJobQueue, JobError};
        use serde::Deserialize;

        #[derive(Deserialize)]
        pub struct ContactForm {
            pub email: String,
            pub message: String,
        }

        async fn contact(
            Extension(queue): Extension<Arc<InMemoryJobQueue>>,
            Json(form): Json<ContactForm>,
        ) -> StatusCode {
            let email = Email::new()
                .to("support@example.com")
                .reply_to(form.email)
                .subject("Contact form")
                .body(form.message);
            // Returns at once; a worker sends the email.
            match dispatch_email(&*queue, &email).await {
                Ok(()) => StatusCode::ACCEPTED,
                Err(JobError::Fatal(_)) => StatusCode::BAD_REQUEST, // the email is not valid
                Err(_) => StatusCode::SERVICE_UNAVAILABLE,          // the queue failed
            }
        }

        pub fn router(queue: Arc<InMemoryJobQueue>) -> Router {
            Router::new()
                .route("/contact", post(contact))
                .layer(Extension(queue))
        }
        // doc-snippet:end
    }
}

#[cfg(feature = "batteries")]
#[path = "support/scaffold_urls.rs"]
mod urls;

#[cfg(feature = "batteries")]
#[allow(dead_code)]
#[rustfmt::skip]
mod sync_main {
    use super::sync_app as myblog;
    use super::urls;

    async fn run() -> Result<(), Box<dyn std::error::Error>> {
        // doc-snippet:start
        // src/main.rs, inside run()
        let _ = dotenvy::dotenv();
        let loaded = rustango::config::Settings::load_from_env();
        let settings = loaded.as_ref().cloned().unwrap_or_default();
        let admin_settings = settings.clone();
        let mailer = rustango::email::from_settings(&settings.mail)?; // NEW: reads [mail]

        rustango::manage::Cli::new()
            .api(urls::api().merge(myblog::contact::router(mailer))) // NEW: mounts /contact
            .try_nest_with(urls::admin_prefix(&settings), move |pool| {
                urls::admin_router(pool, &admin_settings)
            })
            .with_health()
            .with_loaded_settings(loaded)
            .run()
            .await
        // doc-snippet:end
    }
}

#[cfg(feature = "batteries")]
#[allow(dead_code)]
#[rustfmt::skip]
mod queued_main {
    use super::queued_app as myblog;
    use super::urls;
    use std::sync::Arc;
    use rustango::email_jobs::{register_email_job, EmailJobConfig};
    use rustango::jobs::{InMemoryJobQueue, JobQueue};

    async fn run() -> Result<(), Box<dyn std::error::Error>> {
        // doc-snippet:start
        // src/main.rs, inside run(). Add at the top of the file:
        //   use std::sync::Arc;
        //   use rustango::email_jobs::{register_email_job, EmailJobConfig};
        //   use rustango::jobs::{InMemoryJobQueue, JobQueue};
        let _ = dotenvy::dotenv();
        let loaded = rustango::config::Settings::load_from_env();
        let settings = loaded.as_ref().cloned().unwrap_or_default();
        let admin_settings = settings.clone();

        let mailer = rustango::email::from_settings(&settings.mail)?;
        let queue = Arc::new(InMemoryJobQueue::with_workers(4));
        register_email_job(&*queue, EmailJobConfig::new(mailer)).await;
        queue.start().await;
        let draining = Arc::clone(&queue);

        rustango::manage::Cli::new()
            .api(urls::api().merge(myblog::contact::router(queue)))
            .try_nest_with(urls::admin_prefix(&settings), move |pool| {
                urls::admin_router(pool, &admin_settings)
            })
            .with_health()
            .with_loaded_settings(loaded)
            .on_shutdown(move || async move { draining.shutdown().await })
            .run()
            .await
        // doc-snippet:end
    }
}

/// The "Templates" example, with the mailer it sends through.
#[cfg(feature = "admin")]
#[allow(dead_code)]
#[rustfmt::skip]
async fn render_welcome(mailer: rustango::email::BoxedMailer) -> Result<(), Box<dyn std::error::Error>> {
    // doc-snippet:start
    use rustango::email_templates::EmailRenderer;
    use tera::Context;

    // Once, at boot. Share it with handlers as Arc<EmailRenderer>, like the mailer.
    let renderer = EmailRenderer::from_dir("email_templates")?;

    // In the handler:
    let mut ctx = Context::new();
    ctx.insert("name", "Ada");
    let email = renderer.render("welcome", &ctx)?.to("ada@example.com");
    email.send(&*mailer).await?;
    // doc-snippet:end
    Ok(())
}

/// POST `body` as JSON to `/contact` on `app`.
async fn post_contact(app: axum::Router, body: &str) -> axum::http::StatusCode {
    use tower::ServiceExt as _;
    let req = axum::http::Request::post("/contact")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_owned()))
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn the_contact_handler_sends_through_the_extension_mailer() {
    use axum::http::StatusCode;
    let mailer = std::sync::Arc::new(InMemoryMailer::new());
    let app = sync_app::contact::router(mailer.clone());

    let ok = post_contact(
        app.clone(),
        r#"{"email":"ada@example.com","message":"Hello"}"#,
    )
    .await;
    assert_eq!(ok, StatusCode::NO_CONTENT);
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].reply_to.as_deref(), Some("ada@example.com"));
    assert_eq!(sent[0].body, "Hello");

    // A newline in the reply-to address is a bad header, not a retry.
    let bad = post_contact(
        app,
        r#"{"email":"a@example.com\r\nBcc: x@example.com","message":"x"}"#,
    )
    .await;
    assert_eq!(bad, StatusCode::BAD_REQUEST);
}

#[cfg(feature = "jobs")]
#[tokio::test]
async fn the_queued_contact_handler_answers_at_once_and_a_worker_sends() {
    use axum::http::StatusCode;
    use rustango::email_jobs::{register_email_job, EmailJobConfig};
    use rustango::jobs::{InMemoryJobQueue, JobQueue};

    let mailer = std::sync::Arc::new(InMemoryMailer::new());
    let queue = std::sync::Arc::new(InMemoryJobQueue::with_workers(1));
    register_email_job(&*queue, EmailJobConfig::new(mailer.clone())).await;
    queue.start().await;
    let app = queued_app::contact::router(queue.clone());

    let ok = post_contact(
        app.clone(),
        r#"{"email":"ada@example.com","message":"Hello"}"#,
    )
    .await;
    assert_eq!(ok, StatusCode::ACCEPTED);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while mailer.sent().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(mailer.sent().len(), 1, "a worker sent the queued email");

    // An email that fails validation is refused before it is queued.
    let bad = post_contact(
        app,
        r#"{"email":"a@example.com\r\nBcc: x@example.com","message":"x"}"#,
    )
    .await;
    assert_eq!(bad, StatusCode::BAD_REQUEST);
    queue.shutdown().await;
}

/// Every key of the page's `[mail]` block reaches `MailSettings`, in each
/// locale. Read from the page itself, so a renamed key fails here.
#[cfg(feature = "config")]
#[test]
fn the_documented_mail_keys_all_deserialize() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    for dir in ["", "de", "es", "fr"] {
        let page = std::fs::read_to_string(docs.join(dir).join("email.md")).unwrap();
        let block = page
            .split("```toml\n")
            .skip(1)
            .filter_map(|rest| rest.split("```").next())
            .find(|block| block.contains("[mail]"))
            .expect("a toml block with [mail]");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("default.toml"), block).unwrap();
        let mail = rustango::config::Settings::load_from(tmp.path(), "dev")
            .unwrap()
            .mail;
        assert_eq!(mail.backend.as_deref(), Some("smtp"));
        assert_eq!(mail.smtp_host.as_deref(), Some("smtp.example.com"));
        assert_eq!(mail.smtp_port, Some(587));
        assert_eq!(mail.smtp_tls.as_deref(), Some("starttls"));
        assert_eq!(mail.smtp_username.as_deref(), Some("apikey"));
        assert_eq!(mail.smtp_password, None, "the password is not in the file");
        assert_eq!(mail.smtp_timeout_secs, Some(10));
        assert_eq!(
            mail.from_address.as_deref(),
            Some("My App <noreply@example.com>")
        );
        assert_eq!(mail.server_email.as_deref(), Some("ops@example.com"));
        assert_eq!(mail.email_subject_prefix.as_deref(), Some("[My App] "));
        assert_eq!(mail.admins, ["ops@example.com"]);
        assert_eq!(mail.managers, ["team@example.com"]);
    }
}
