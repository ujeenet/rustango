//! Laravel-shape `Mailable` trait — declare an email type as a struct
//! that owns its own template + recipient logic. Pairs with
//! [`crate::email_templates::EmailRenderer`] (template rendering) and
//! [`crate::email_jobs`] (off-request delivery).
//!
//! ## Why
//!
//! Without this, building one email looks like:
//!
//! ```ignore
//! let mut ctx = Context::new();
//! ctx.insert("name", &user.name);
//! ctx.insert("url", &reset_url);
//! let email = renderer.render("password_reset", &ctx)?
//!     .to(&user.email)
//!     .from("noreply@example.com");
//! mailer.send(&email).await?;
//! ```
//!
//! With a Mailable, the email type owns the template + recipient
//! logic, so call sites are a one-liner:
//!
//! ```ignore
//! PasswordReset { user: user.clone(), reset_url }
//!     .send(&renderer, &mailer).await?;
//!
//! // Or via the job queue:
//! PasswordReset { user, reset_url }.dispatch(&renderer, &queue).await?;
//! ```
//!
//! ## Defining a Mailable
//!
//! ```ignore
//! use rustango::mailable::Mailable;
//! use rustango::email::Email;
//! use rustango::email_templates::EmailRenderer;
//! use tera::Context;
//!
//! struct PasswordReset {
//!     pub user: User,
//!     pub reset_url: String,
//! }
//!
//! impl Mailable for PasswordReset {
//!     const TEMPLATE: &'static str = "password_reset";
//!
//!     fn build(&self, base: Email) -> Email {
//!         base.to(&self.user.email)
//!             .from("noreply@example.com")
//!             .reply_to("support@example.com")
//!     }
//!
//!     fn context(&self) -> Context {
//!         let mut c = Context::new();
//!         c.insert("name", &self.user.name);
//!         c.insert("url", &self.reset_url);
//!         c
//!     }
//! }
//! ```

use tera::Context;

use crate::email::{BoxedMailer, Email, MailError};
use crate::email_templates::{EmailRenderer, EmailRenderError};

#[derive(Debug, thiserror::Error)]
pub enum MailableError {
    #[error("render: {0}")]
    Render(#[from] EmailRenderError),
    #[error("send: {0}")]
    Send(#[from] MailError),
}

/// One sendable email type. The struct holds whatever per-instance
/// state it needs (recipient, IDs, URLs) and `Mailable` glues it
/// to a Tera template + recipient + sender logic.
pub trait Mailable {
    /// Template basename — the renderer looks for
    /// `{TEMPLATE}.subject.txt` + `{TEMPLATE}.txt` (+ optional
    /// `{TEMPLATE}.html`) under this name.
    const TEMPLATE: &'static str;

    /// Mutate the rendered [`Email`] to set recipients, sender,
    /// reply-to, custom headers, etc. The default implementation
    /// returns the rendered email unchanged — override to add at
    /// least one `.to(...)` call.
    fn build(&self, base: Email) -> Email {
        base
    }

    /// Build the Tera context the template renders against. Default:
    /// empty context (only useful for templates with no variables).
    fn context(&self) -> Context {
        Context::new()
    }

    /// Render the email via `renderer` and return it ready-to-send.
    ///
    /// # Errors
    /// `Render(_)` for any template error (missing / parse failure).
    fn render(&self, renderer: &EmailRenderer) -> Result<Email, MailableError> {
        let ctx = self.context();
        let base = renderer.render(Self::TEMPLATE, &ctx)?;
        Ok(self.build(base))
    }

    /// Render + send synchronously.
    ///
    /// # Errors
    /// `Render(_)` for template errors, `Send(_)` for mailer errors
    /// (refused recipient, transport failure, etc.).
    fn send<'a>(
        &'a self,
        renderer: &'a EmailRenderer,
        mailer: &'a BoxedMailer,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailableError>> + Send + 'a>>
    where
        Self: Sync,
    {
        Box::pin(async move {
            let email = self.render(renderer)?;
            mailer.send(&email).await?;
            Ok(())
        })
    }

    /// Render now, dispatch via the [`crate::jobs`] queue. Returns
    /// immediately; delivery happens on a worker.
    ///
    /// # Errors
    /// `Render(_)` for template errors. Queue errors bubble up via
    /// the inner [`crate::jobs::JobError`] mapped into a `Send`
    /// variant (string-shaped).
    #[cfg(feature = "jobs")]
    fn dispatch<'a, Q>(
        &'a self,
        renderer: &'a EmailRenderer,
        queue: &'a Q,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailableError>> + Send + 'a>>
    where
        Self: Sync,
        Q: crate::jobs::JobQueue + Sync,
    {
        Box::pin(async move {
            let email = self.render(renderer)?;
            crate::email_jobs::dispatch_email(queue, &email)
                .await
                .map_err(|e| {
                    MailableError::Send(crate::email::MailError::Transport(e.to_string()))
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::InMemoryMailer;
    use std::sync::Arc;

    fn renderer() -> EmailRenderer {
        EmailRenderer::from_pairs(vec![
            ("welcome.subject.txt", "Welcome, {{ name }}"),
            ("welcome.txt", "Hi {{ name }}, your code is {{ code }}"),
            ("welcome.html", "<p>Hi {{ name }}, code: <b>{{ code }}</b></p>"),
        ])
        .unwrap()
    }

    struct WelcomeMail {
        name: String,
        code: u32,
        recipient: String,
    }

    impl Mailable for WelcomeMail {
        const TEMPLATE: &'static str = "welcome";

        fn build(&self, base: Email) -> Email {
            base.to(&self.recipient).from("noreply@x.com")
        }

        fn context(&self) -> Context {
            let mut c = Context::new();
            c.insert("name", &self.name);
            c.insert("code", &self.code);
            c
        }
    }

    #[test]
    fn render_picks_up_template_subject_and_body() {
        let r = renderer();
        let m = WelcomeMail {
            name: "Alice".into(),
            code: 42,
            recipient: "alice@x.com".into(),
        };
        let email = m.render(&r).unwrap();
        assert_eq!(email.subject, "Welcome, Alice");
        assert_eq!(email.body, "Hi Alice, your code is 42");
        assert_eq!(email.html_body.as_deref(), Some("<p>Hi Alice, code: <b>42</b></p>"));
        assert_eq!(email.to, vec!["alice@x.com"]);
        assert_eq!(email.from.as_deref(), Some("noreply@x.com"));
    }

    #[tokio::test]
    async fn send_pushes_through_mailer() {
        let r = renderer();
        // Hold a typed handle for inspection AND the Arc<dyn Mailer>
        // we hand to send(); both point at the same InMemoryMailer.
        let im = Arc::new(InMemoryMailer::new());
        let mailer: BoxedMailer = im.clone();
        let m = WelcomeMail {
            name: "Bob".into(),
            code: 7,
            recipient: "bob@x.com".into(),
        };
        m.send(&r, &mailer).await.unwrap();
        assert_eq!(im.count(), 1);
        let sent = im.sent();
        assert_eq!(sent[0].subject, "Welcome, Bob");
        assert_eq!(sent[0].to, vec!["bob@x.com"]);
    }

    #[test]
    fn render_fails_with_clear_error_for_missing_template() {
        struct MissingMail;
        impl Mailable for MissingMail {
            const TEMPLATE: &'static str = "no_such_template";
            fn build(&self, base: Email) -> Email {
                base.to("x@x.com")
            }
        }

        let r = renderer(); // doesn't include "no_such_template"
        let err = MissingMail.render(&r).unwrap_err();
        match err {
            MailableError::Render(EmailRenderError::Missing(name)) => {
                assert_eq!(name, "no_such_template.subject.txt");
            }
            other => panic!("expected Render::Missing, got: {other:?}"),
        }
    }

    #[test]
    fn build_default_returns_base_unchanged() {
        struct BareMail;
        impl Mailable for BareMail {
            const TEMPLATE: &'static str = "welcome";
            fn context(&self) -> Context {
                let mut c = Context::new();
                c.insert("name", "Anon");
                c.insert("code", &0);
                c
            }
        }
        let r = renderer();
        let email = BareMail.render(&r).unwrap();
        // Default build() leaves recipients empty.
        assert!(email.to.is_empty());
        assert!(email.from.is_none());
        // But template still rendered.
        assert_eq!(email.subject, "Welcome, Anon");
    }
}
