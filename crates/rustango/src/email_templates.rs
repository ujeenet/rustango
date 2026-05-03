//! Tera-rendered email helpers — bridge the existing
//! [`crate::email`] mailer trait and the Tera templating engine
//! (already a dep on the `admin` feature).
//!
//! Each email is a set of three sibling templates in a directory:
//!
//! ```text
//! email_templates/
//!   welcome.subject.txt   -- single-line subject (whitespace trimmed)
//!   welcome.txt           -- plain-text body  (always required)
//!   welcome.html          -- HTML body        (optional)
//! ```
//!
//! Subject + plain text are required; HTML is added when present so
//! mail clients that prefer HTML get it and plain-text fallbacks
//! still work.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::email_templates::EmailRenderer;
//! use rustango::email::Mailer;
//! use tera::Context;
//!
//! let renderer = EmailRenderer::from_dir("./email_templates")?;
//!
//! let mut ctx = Context::new();
//! ctx.insert("name", "Alice");
//! ctx.insert("url", "https://app.example.com/verify/abc");
//!
//! let email = renderer.render("welcome", &ctx)?
//!     .from("noreply@example.com")
//!     .to("alice@example.com");
//!
//! mailer.send(email).await?;
//! ```
//!
//! ## Inline templates (tests + ad-hoc)
//!
//! ```ignore
//! let renderer = EmailRenderer::from_pairs(vec![
//!     ("welcome.subject.txt", "Welcome, {{ name }}"),
//!     ("welcome.txt",          "Hi {{ name }}, click {{ url }} to verify."),
//!     ("welcome.html",         "<p>Hi {{ name }}, <a href=\"{{ url }}\">verify</a>.</p>"),
//! ])?;
//! ```

use std::path::Path;

use tera::{Context, Tera};

use crate::email::Email;

#[derive(Debug, thiserror::Error)]
pub enum EmailRenderError {
    #[error("template error: {0}")]
    Tera(String),
    #[error("required template missing: {0}")]
    Missing(String),
}

impl From<tera::Error> for EmailRenderError {
    fn from(e: tera::Error) -> Self {
        Self::Tera(e.to_string())
    }
}

/// Holds a Tera engine plus per-instance rendering helpers.
///
/// Cheap to clone (the inner `Tera` is wrapped in `Arc` internally
/// when shared across handlers — pass `Arc<EmailRenderer>` if you
/// want to avoid the `Tera` clone cost).
pub struct EmailRenderer {
    tera: Tera,
}

impl EmailRenderer {
    /// Load every `*.txt`, `*.html`, `*.subject.txt` under `dir`.
    /// Subdirectories are walked.
    ///
    /// # Errors
    /// Returns the underlying Tera error when a template doesn't
    /// parse, or the directory glob can't be evaluated.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, EmailRenderError> {
        let glob = format!("{}/**/*", dir.as_ref().display());
        let tera = Tera::new(&glob)?;
        Ok(Self { tera })
    }

    /// Build from in-memory `(name, source)` pairs. Useful for
    /// tests and one-off scripts that don't want a templates dir.
    ///
    /// # Errors
    /// Returns the underlying Tera error if a template doesn't
    /// parse.
    pub fn from_pairs(pairs: Vec<(&str, &str)>) -> Result<Self, EmailRenderError> {
        let mut tera = Tera::default();
        // Disable autoescape — email templates are not always HTML, and
        // the user is responsible for choosing the right escape strategy
        // per template.
        tera.autoescape_on(Vec::new());
        for (name, source) in pairs {
            tera.add_raw_template(name, source)?;
        }
        Ok(Self { tera })
    }

    /// Borrow the inner Tera. Useful when you want to register
    /// custom filters / functions.
    #[must_use]
    pub fn tera(&self) -> &Tera {
        &self.tera
    }

    /// Mutable borrow for filter/function registration at startup.
    pub fn tera_mut(&mut self) -> &mut Tera {
        &mut self.tera
    }

    /// Render `{name}.subject.txt`, `{name}.txt`, and (optionally)
    /// `{name}.html` and pack them into an [`Email`]. The returned
    /// email has only `subject`, `body`, and `html_body` populated —
    /// chain `.to()` / `.from()` / etc. before sending.
    ///
    /// # Errors
    /// `Missing(name)` when subject or text body is absent;
    /// `Tera(_)` for any template error.
    pub fn render(&self, name: &str, context: &Context) -> Result<Email, EmailRenderError> {
        let subject_name = format!("{name}.subject.txt");
        let text_name = format!("{name}.txt");
        let html_name = format!("{name}.html");

        let subject = self
            .tera
            .render(&subject_name, context)
            .map_err(|e| match underlying_kind(&e) {
                TemplateMissing::Yes => EmailRenderError::Missing(subject_name.clone()),
                TemplateMissing::No => e.into(),
            })?
            .trim()
            .to_owned();

        let body = self
            .tera
            .render(&text_name, context)
            .map_err(|e| match underlying_kind(&e) {
                TemplateMissing::Yes => EmailRenderError::Missing(text_name.clone()),
                TemplateMissing::No => e.into(),
            })?;

        let mut email = Email::new().subject(subject).body(body);

        // HTML body is optional — render only when the template exists.
        if self.tera.get_template_names().any(|t| t == html_name) {
            let html = self.tera.render(&html_name, context)?;
            email = email.html_body(html);
        }
        Ok(email)
    }
}

enum TemplateMissing {
    Yes,
    No,
}

fn underlying_kind(e: &tera::Error) -> TemplateMissing {
    let msg = e.to_string();
    if msg.contains("not found") || msg.contains("does not exist") {
        TemplateMissing::Yes
    } else {
        TemplateMissing::No
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tera::Context;

    fn ctx_alice() -> Context {
        let mut c = Context::new();
        c.insert("name", "Alice");
        c.insert("url", "https://example.com/verify/abc");
        c
    }

    #[test]
    fn renders_subject_and_text_body_only() {
        let r = EmailRenderer::from_pairs(vec![
            ("welcome.subject.txt", "Welcome, {{ name }}"),
            ("welcome.txt", "Hi {{ name }}, visit {{ url }}"),
        ])
        .unwrap();
        let email = r.render("welcome", &ctx_alice()).unwrap();
        assert_eq!(email.subject, "Welcome, Alice");
        assert_eq!(
            email.body,
            "Hi Alice, visit https://example.com/verify/abc"
        );
        assert!(email.html_body.is_none());
    }

    #[test]
    fn renders_html_body_when_present() {
        let r = EmailRenderer::from_pairs(vec![
            ("welcome.subject.txt", "Hi"),
            ("welcome.txt", "Hi {{ name }}"),
            ("welcome.html", "<p>Hi {{ name }}</p>"),
        ])
        .unwrap();
        let email = r.render("welcome", &ctx_alice()).unwrap();
        assert_eq!(email.html_body.as_deref(), Some("<p>Hi Alice</p>"));
    }

    #[test]
    fn missing_subject_returns_clear_error() {
        let r = EmailRenderer::from_pairs(vec![("welcome.txt", "x")]).unwrap();
        let err = r.render("welcome", &Context::new()).unwrap_err();
        match err {
            EmailRenderError::Missing(name) => assert_eq!(name, "welcome.subject.txt"),
            other => panic!("expected Missing error, got: {other:?}"),
        }
    }

    #[test]
    fn missing_text_body_returns_clear_error() {
        let r = EmailRenderer::from_pairs(vec![("welcome.subject.txt", "x")]).unwrap();
        let err = r.render("welcome", &Context::new()).unwrap_err();
        match err {
            EmailRenderError::Missing(name) => assert_eq!(name, "welcome.txt"),
            other => panic!("expected Missing error, got: {other:?}"),
        }
    }

    #[test]
    fn subject_is_trimmed() {
        // Subject lines often gain leading whitespace from indentation
        // in templates — trim it.
        let r = EmailRenderer::from_pairs(vec![
            ("hi.subject.txt", "  Hello\n"),
            ("hi.txt", "body"),
        ])
        .unwrap();
        let e = r.render("hi", &Context::new()).unwrap();
        assert_eq!(e.subject, "Hello");
    }

    #[test]
    fn template_syntax_error_propagates() {
        // Garbled template — Tera should reject at parse time.
        let r = EmailRenderer::from_pairs(vec![
            ("hi.subject.txt", "{{ unbalanced"),
            ("hi.txt", "x"),
        ]);
        assert!(r.is_err(), "parse error should bubble out of from_pairs");
    }

    #[test]
    fn from_dir_loads_files_and_renders() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("ping.subject.txt"), "Ping {{ n }}").unwrap();
        std::fs::write(dir.path().join("ping.txt"), "n={{ n }}").unwrap();
        let r = EmailRenderer::from_dir(dir.path()).unwrap();

        let mut c = Context::new();
        c.insert("n", &7);
        let email = r.render("ping", &c).unwrap();
        assert_eq!(email.subject, "Ping 7");
        assert_eq!(email.body, "n=7");
    }

    #[test]
    fn tera_mut_lets_caller_register_filters() {
        let mut r = EmailRenderer::from_pairs(vec![
            ("hi.subject.txt", "x"),
            ("hi.txt", "x"),
        ])
        .unwrap();
        // Register a no-op filter; just verifying the access works.
        r.tera_mut().register_filter("noop", |v: &tera::Value, _: &_| Ok(v.clone()));
        // Smoke test that rendering still works.
        let _ = r.render("hi", &Context::new()).unwrap();
    }

    #[test]
    fn renders_against_complex_context() {
        let r = EmailRenderer::from_pairs(vec![
            ("o.subject.txt", "Order {{ order.id }}"),
            (
                "o.txt",
                "{{ user.name }} ordered {{ order.items | length }} items.",
            ),
        ])
        .unwrap();
        let mut c = Context::new();
        c.insert("user", &serde_json::json!({"name": "Alice"}));
        c.insert(
            "order",
            &serde_json::json!({"id": 42, "items": [1, 2, 3]}),
        );
        let e = r.render("o", &c).unwrap();
        assert_eq!(e.subject, "Order 42");
        assert_eq!(e.body, "Alice ordered 3 items.");
    }
}
