//! Tera-rendered emails: joins the [`crate::email`] mailer to the Tera
//! template engine.
//!
//! Each email is up to three templates side by side:
//!
//! ```text
//! email_templates/
//!   welcome.subject.txt   -- single-line subject (whitespace trimmed)
//!   welcome.txt           -- plain-text body  (always required)
//!   welcome.html          -- HTML body        (optional)
//! ```
//!
//! The subject and the plain-text body are required. The HTML body is
//! added when the file exists, so HTML clients get HTML and the rest
//! still get text.
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

/// Your email templates, loaded into two Tera engines: HTML bodies
/// render escaped, the subject and plain-text body render raw (#1721).
/// Share it between handlers as `Arc<EmailRenderer>`.
pub struct EmailRenderer {
    html: Tera,
    text: Tera,
}

impl EmailRenderer {
    /// Load every template under `dir`, subdirectories included.
    ///
    /// # Errors
    /// The Tera error when a template does not parse, or the glob
    /// cannot be read.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, EmailRenderError> {
        let glob = format!("{}/**/*", dir.as_ref().display());
        Ok(Self::from_html_tera(
            crate::template_extensions::html_tera_from_glob(&glob)?,
        ))
    }

    /// Build from `(name, source)` pairs held in memory. Good for
    /// tests and small scripts with no template directory.
    ///
    /// # Errors
    /// The Tera error when a template does not parse.
    pub fn from_pairs(pairs: Vec<(&str, &str)>) -> Result<Self, EmailRenderError> {
        let mut tera = crate::template_extensions::html_tera();
        tera.add_raw_templates(pairs)?;
        Ok(Self::from_html_tera(tera))
    }

    fn from_html_tera(html: Tera) -> Self {
        let mut text = html.clone();
        text.autoescape_on(Vec::new());
        Self { html, text }
    }

    /// The HTML (escaping) engine. The text engine has the same templates.
    #[must_use]
    pub fn tera(&self) -> &Tera {
        &self.html
    }

    /// Change both engines, e.g. to register filters at startup.
    pub fn configure(&mut self, f: impl Fn(&mut Tera)) {
        f(&mut self.html);
        f(&mut self.text);
    }

    /// Render `{name}.subject.txt`, `{name}.txt` and, if it exists,
    /// `{name}.html` into an [`Email`]. Only the subject and bodies
    /// are set, so chain `.from()` and `.to()` before sending.
    ///
    /// # Errors
    /// `Missing(name)` when the subject or text body is absent,
    /// `Tera(_)` for any other template error.
    pub fn render(&self, name: &str, context: &Context) -> Result<Email, EmailRenderError> {
        let subject_name = format!("{name}.subject.txt");
        let text_name = format!("{name}.txt");
        let html_name = format!("{name}.html");

        let subject = self
            .text
            .render(&subject_name, context)
            .map_err(|e| match underlying_kind(&e) {
                TemplateMissing::Yes => EmailRenderError::Missing(subject_name.clone()),
                TemplateMissing::No => e.into(),
            })?
            .trim()
            .to_owned();

        let body =
            self.text
                .render(&text_name, context)
                .map_err(|e| match underlying_kind(&e) {
                    TemplateMissing::Yes => EmailRenderError::Missing(text_name.clone()),
                    TemplateMissing::No => e.into(),
                })?;

        let mut email = Email::new().subject(subject).body(body);

        // The HTML body is optional.
        if self.html.get_template_names().any(|t| t == html_name) {
            let html = self.html.render(&html_name, context)?;
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
        assert_eq!(email.body, "Hi Alice, visit https://example.com/verify/abc");
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
        // Template indentation often leaks into the subject line.
        let r =
            EmailRenderer::from_pairs(vec![("hi.subject.txt", "  Hello\n"), ("hi.txt", "body")])
                .unwrap();
        let e = r.render("hi", &Context::new()).unwrap();
        assert_eq!(e.subject, "Hello");
    }

    #[test]
    fn template_syntax_error_propagates() {
        // Tera rejects a broken template at parse time.
        let r =
            EmailRenderer::from_pairs(vec![("hi.subject.txt", "{{ unbalanced"), ("hi.txt", "x")]);
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
    fn configure_registers_filters_on_both_engines() {
        let mut r = EmailRenderer::from_pairs(vec![
            ("hi.subject.txt", "{{ 1 | noop }}"),
            ("hi.txt", "{{ 2 | noop }}"),
            ("hi.html", "{{ 3 | noop }}"),
        ])
        .unwrap();
        r.configure(|t| t.register_filter("noop", |v: &tera::Value, _: &_| Ok(v.clone())));
        let e = r.render("hi", &Context::new()).unwrap();
        assert_eq!((e.subject.as_str(), e.body.as_str()), ("1", "2"));
        assert_eq!(e.html_body.as_deref(), Some("3"));
    }

    #[test]
    fn html_body_escapes_and_text_body_does_not() {
        let r = EmailRenderer::from_pairs(vec![
            ("x.subject.txt", "Re: {{ v }}"),
            ("x.txt", "{{ v }}"),
            ("x.html", "<p>{{ v }}</p>"),
        ])
        .unwrap();
        let mut c = Context::new();
        c.insert("v", "<script>&");
        let e = r.render("x", &c).unwrap();
        assert_eq!(e.html_body.as_deref(), Some("<p>&lt;script&gt;&amp;</p>"));
        assert_eq!(e.body, "<script>&");
        assert_eq!(e.subject, "Re: <script>&");
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
        c.insert("order", &serde_json::json!({"id": 42, "items": [1, 2, 3]}));
        let e = r.render("o", &c).unwrap();
        assert_eq!(e.subject, "Order 42");
        assert_eq!(e.body, "Alice ordered 3 items.");
    }
}
