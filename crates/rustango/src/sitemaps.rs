//! XML sitemap rendering — Django's `django.contrib.sitemaps`.
//!
//! Generates `sitemap.xml` and `sitemap_index.xml` payloads conforming
//! to the [sitemaps.org protocol](https://www.sitemaps.org/protocol.html).
//! Wire-up is the caller's job: register a handler at `/sitemap.xml`
//! that calls [`render_sitemap`] / [`render_sitemap_index`] and
//! returns the result with `Content-Type: application/xml`.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::sitemaps::{Sitemap, SitemapEntry, ChangeFreq, render_sitemap};
//! use chrono::Utc;
//!
//! struct ArticleSitemap;
//! impl Sitemap for ArticleSitemap {
//!     fn entries(&self) -> Vec<SitemapEntry> {
//!         vec![
//!             SitemapEntry::new("https://example.com/articles/hello")
//!                 .with_lastmod(Utc::now())
//!                 .with_changefreq(ChangeFreq::Weekly)
//!                 .with_priority(0.8),
//!         ]
//!     }
//! }
//!
//! let xml = render_sitemap(&ArticleSitemap);
//! ```
//!
//! ## Sitemap index
//!
//! Large sites split the sitemap across multiple files. [`render_sitemap_index`]
//! produces the index XML that points at child sitemap files:
//!
//! ```ignore
//! use rustango::sitemaps::{render_sitemap_index, SitemapIndexEntry};
//!
//! let xml = render_sitemap_index(&[
//!     SitemapIndexEntry::new("https://example.com/sitemap-articles.xml"),
//!     SitemapIndexEntry::new("https://example.com/sitemap-pages.xml"),
//! ]);
//! ```
//!
//! Issue #57 (smaller contrib apps).

use chrono::{DateTime, Utc};

// ------------------------------------------------------------------ ChangeFreq

/// `<changefreq>` values per sitemaps.org. Renders lower-cased as the
/// protocol requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeFreq {
    Always,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Never,
}

impl ChangeFreq {
    /// Protocol-conformant lower-cased token.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
            Self::Never => "never",
        }
    }
}

// ------------------------------------------------------------------ SitemapEntry

/// One `<url>` element in a sitemap.
#[derive(Debug, Clone)]
pub struct SitemapEntry {
    /// Absolute `<loc>` URL. The protocol requires this be a full URL
    /// (scheme + host + path); relative paths render as-is but
    /// crawlers will reject them.
    pub loc: String,
    pub lastmod: Option<DateTime<Utc>>,
    pub changefreq: Option<ChangeFreq>,
    /// `<priority>` ∈ [0.0, 1.0]. Out-of-range values clamp at render
    /// time.
    pub priority: Option<f64>,
}

impl SitemapEntry {
    /// Build an entry with just the URL. Add lastmod / changefreq /
    /// priority via the `with_*` setters.
    #[must_use]
    pub fn new(loc: impl Into<String>) -> Self {
        Self {
            loc: loc.into(),
            lastmod: None,
            changefreq: None,
            priority: None,
        }
    }

    #[must_use]
    pub fn with_lastmod(mut self, when: DateTime<Utc>) -> Self {
        self.lastmod = Some(when);
        self
    }

    #[must_use]
    pub fn with_changefreq(mut self, cf: ChangeFreq) -> Self {
        self.changefreq = Some(cf);
        self
    }

    /// Clamps to `[0.0, 1.0]` on render — pass any `f64`, render handles
    /// the bound.
    #[must_use]
    pub fn with_priority(mut self, p: f64) -> Self {
        self.priority = Some(p);
        self
    }
}

// ------------------------------------------------------------------ SitemapIndexEntry

/// One `<sitemap>` element inside a sitemap index.
#[derive(Debug, Clone)]
pub struct SitemapIndexEntry {
    pub loc: String,
    pub lastmod: Option<DateTime<Utc>>,
}

impl SitemapIndexEntry {
    #[must_use]
    pub fn new(loc: impl Into<String>) -> Self {
        Self {
            loc: loc.into(),
            lastmod: None,
        }
    }

    #[must_use]
    pub fn with_lastmod(mut self, when: DateTime<Utc>) -> Self {
        self.lastmod = Some(when);
        self
    }
}

// ------------------------------------------------------------------ Sitemap

/// One sitemap source. Implementations return a fresh list of
/// entries on each call — caching is the caller's responsibility.
pub trait Sitemap: Send + Sync {
    fn entries(&self) -> Vec<SitemapEntry>;
}

// Blanket impl: `Vec<SitemapEntry>` is itself a sitemap source so
// callers don't have to declare a struct for one-off cases.
impl Sitemap for Vec<SitemapEntry> {
    fn entries(&self) -> Vec<SitemapEntry> {
        self.clone()
    }
}

// ------------------------------------------------------------------ render

/// Render a `<urlset>` document. Output is well-formed XML 1.0 with
/// the sitemaps.org schema. Each `<url>` element emits `<loc>` (always),
/// `<lastmod>` (when set; ISO-8601 UTC), `<changefreq>` (when set), and
/// `<priority>` (when set; clamped to `[0.0, 1.0]`, 1-decimal).
#[must_use]
pub fn render_sitemap<S: Sitemap + ?Sized>(s: &S) -> String {
    let entries = s.entries();
    let mut out = String::with_capacity(256 + entries.len() * 128);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for e in &entries {
        out.push_str("  <url>\n");
        push_text_element(&mut out, "loc", &e.loc, 4);
        if let Some(ts) = e.lastmod {
            push_text_element(&mut out, "lastmod", &format_iso8601(ts), 4);
        }
        if let Some(cf) = e.changefreq {
            push_text_element(&mut out, "changefreq", cf.as_str(), 4);
        }
        if let Some(p) = e.priority {
            let clamped = p.clamp(0.0, 1.0);
            push_text_element(&mut out, "priority", &format!("{clamped:.1}"), 4);
        }
        out.push_str("  </url>\n");
    }
    out.push_str("</urlset>\n");
    out
}

/// Render a `<sitemapindex>` document — points crawlers at multiple
/// child sitemaps.
#[must_use]
pub fn render_sitemap_index(entries: &[SitemapIndexEntry]) -> String {
    let mut out = String::with_capacity(256 + entries.len() * 96);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for e in entries {
        out.push_str("  <sitemap>\n");
        push_text_element(&mut out, "loc", &e.loc, 4);
        if let Some(ts) = e.lastmod {
            push_text_element(&mut out, "lastmod", &format_iso8601(ts), 4);
        }
        out.push_str("  </sitemap>\n");
    }
    out.push_str("</sitemapindex>\n");
    out
}

// ------------------------------------------------------------------ helpers

/// Emit `  <name>escaped-text</name>\n` with `indent` leading spaces.
fn push_text_element(out: &mut String, name: &str, text: &str, indent: usize) {
    for _ in 0..indent {
        out.push(' ');
    }
    out.push('<');
    out.push_str(name);
    out.push('>');
    escape_xml_text(out, text);
    out.push_str("</");
    out.push_str(name);
    out.push_str(">\n");
}

/// Sitemap loc URLs occasionally include `&` (query strings), `<`/`>`/
/// `"` (escaped HTML in pathological cases), and `'` (path components).
/// Escape the XML5 set so the document remains well-formed.
fn escape_xml_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS+00:00` — the sitemaps.org-recommended format
/// (W3C Date and Time Formats, full datetime). Chrono's default
/// `to_rfc3339` already produces this shape; we just call it.
fn format_iso8601(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(y: i32, mo: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn entry_builder_chains() {
        let e = SitemapEntry::new("https://example.com/x")
            .with_lastmod(ts(2025, 1, 2))
            .with_changefreq(ChangeFreq::Weekly)
            .with_priority(0.7);
        assert_eq!(e.loc, "https://example.com/x");
        assert!(e.lastmod.is_some());
        assert_eq!(e.changefreq, Some(ChangeFreq::Weekly));
        assert_eq!(e.priority, Some(0.7));
    }

    #[test]
    fn render_emits_xml_declaration_and_urlset() {
        let entries: Vec<SitemapEntry> = vec![SitemapEntry::new("https://example.com/")];
        let xml = render_sitemap(&entries);
        assert!(
            xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"),
            "XML declaration missing: {xml}"
        );
        assert!(xml.contains("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">"));
        assert!(xml.contains("<loc>https://example.com/</loc>"));
        assert!(xml.ends_with("</urlset>\n"));
    }

    #[test]
    fn render_omits_unset_optional_fields() {
        let entries: Vec<SitemapEntry> = vec![SitemapEntry::new("https://example.com/")];
        let xml = render_sitemap(&entries);
        assert!(!xml.contains("<lastmod>"));
        assert!(!xml.contains("<changefreq>"));
        assert!(!xml.contains("<priority>"));
    }

    #[test]
    fn render_emits_all_fields_when_set() {
        let entries: Vec<SitemapEntry> = vec![SitemapEntry::new("https://example.com/article/42")
            .with_lastmod(ts(2025, 3, 14))
            .with_changefreq(ChangeFreq::Daily)
            .with_priority(0.9)];
        let xml = render_sitemap(&entries);
        assert!(xml.contains("<loc>https://example.com/article/42</loc>"));
        assert!(xml.contains("<lastmod>2025-03-14T12:00:00Z</lastmod>"));
        assert!(xml.contains("<changefreq>daily</changefreq>"));
        assert!(xml.contains("<priority>0.9</priority>"));
    }

    #[test]
    fn render_clamps_priority_to_0_1_range() {
        let entries: Vec<SitemapEntry> = vec![
            SitemapEntry::new("https://example.com/a").with_priority(2.5), // → 1.0
            SitemapEntry::new("https://example.com/b").with_priority(-1.0), // → 0.0
            SitemapEntry::new("https://example.com/c").with_priority(0.5), // → 0.5
        ];
        let xml = render_sitemap(&entries);
        assert!(xml.contains("<priority>1.0</priority>"));
        assert!(xml.contains("<priority>0.0</priority>"));
        assert!(xml.contains("<priority>0.5</priority>"));
    }

    #[test]
    fn render_escapes_xml_special_chars_in_loc() {
        let entries: Vec<SitemapEntry> = vec![SitemapEntry::new(
            "https://example.com/search?q=rust&lang=en&t=<all>",
        )];
        let xml = render_sitemap(&entries);
        assert!(
            xml.contains("https://example.com/search?q=rust&amp;lang=en&amp;t=&lt;all&gt;"),
            "expected escaped &, <, >: {xml}"
        );
        // Raw & must NOT appear in the document body.
        assert!(
            !xml.contains("q=rust&lang"),
            "unescaped ampersand in: {xml}"
        );
    }

    #[test]
    fn changefreq_renders_lowercase() {
        for (cf, expected) in [
            (ChangeFreq::Always, "always"),
            (ChangeFreq::Hourly, "hourly"),
            (ChangeFreq::Daily, "daily"),
            (ChangeFreq::Weekly, "weekly"),
            (ChangeFreq::Monthly, "monthly"),
            (ChangeFreq::Yearly, "yearly"),
            (ChangeFreq::Never, "never"),
        ] {
            assert_eq!(cf.as_str(), expected);
        }
    }

    #[test]
    fn empty_sitemap_renders_valid_urlset() {
        let entries: Vec<SitemapEntry> = vec![];
        let xml = render_sitemap(&entries);
        assert!(xml.contains("<urlset"));
        assert!(xml.contains("</urlset>"));
        // No <url> entries.
        assert!(!xml.contains("<url>"));
    }

    #[test]
    fn sitemap_index_renders() {
        let xml = render_sitemap_index(&[
            SitemapIndexEntry::new("https://example.com/sitemap-articles.xml")
                .with_lastmod(ts(2025, 5, 1)),
            SitemapIndexEntry::new("https://example.com/sitemap-pages.xml"),
        ]);
        assert!(
            xml.contains("<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">")
        );
        assert!(xml.contains("<sitemap>"));
        assert!(xml.contains("<loc>https://example.com/sitemap-articles.xml</loc>"));
        assert!(xml.contains("<lastmod>2025-05-01T12:00:00Z</lastmod>"));
        assert!(xml.contains("<loc>https://example.com/sitemap-pages.xml</loc>"));
        assert!(xml.ends_with("</sitemapindex>\n"));
    }

    #[test]
    fn custom_sitemap_impl() {
        struct ArticleSitemap;
        impl Sitemap for ArticleSitemap {
            fn entries(&self) -> Vec<SitemapEntry> {
                vec![
                    SitemapEntry::new("https://example.com/articles/hello").with_priority(0.8),
                    SitemapEntry::new("https://example.com/articles/world"),
                ]
            }
        }
        let xml = render_sitemap(&ArticleSitemap);
        assert!(xml.contains("<loc>https://example.com/articles/hello</loc>"));
        assert!(xml.contains("<priority>0.8</priority>"));
        assert!(xml.contains("<loc>https://example.com/articles/world</loc>"));
    }

    #[test]
    fn iso8601_format_uses_z_for_utc() {
        // Sitemaps.org accepts the W3C subset. Chrono's
        // `to_rfc3339_opts(..., true)` emits `Z` for UTC zero offset
        // rather than `+00:00`. Pin the shape.
        let s = format_iso8601(ts(2025, 1, 2));
        assert_eq!(s, "2025-01-02T12:00:00Z");
    }
}
