//! RSS 2.0 + Atom 1.0 feed rendering — `django.contrib.syndication`.
//!
//! Build a [`Feed`] (channel metadata + a `Vec<FeedItem>`) and call
//! [`render_rss`] for an RSS 2.0 document or [`render_atom`] for an
//! Atom 1.0 document. Wire-up is the caller's job: register a handler
//! at `/feed.rss` (or `/feed.atom`) and return the result with
//! `Content-Type: application/rss+xml` (or `application/atom+xml`).
//!
//! ```ignore
//! use rustango::syndication::{Feed, FeedItem, render_rss};
//! use chrono::Utc;
//!
//! let feed = Feed {
//!     title: "Example blog".into(),
//!     link: "https://example.com/".into(),
//!     description: "Latest articles.".into(),
//!     language: Some("en".into()),
//!     items: vec![
//!         FeedItem::new("Hello, world", "https://example.com/articles/hello")
//!             .with_description("First post")
//!             .with_pub_date(Utc::now())
//!             .with_author("alice@example.com"),
//!     ],
//! };
//! let xml = render_rss(&feed);
//! ```
//!
//! Issue #57 (smaller contrib apps).

use chrono::{DateTime, Utc};

// ------------------------------------------------------------------ Feed / FeedItem

/// Channel-level metadata for a feed.
#[derive(Debug, Clone)]
pub struct Feed {
    pub title: String,
    /// Canonical site URL — RSS `<link>`, Atom self link.
    pub link: String,
    pub description: String,
    /// ISO-639-1 language tag (e.g. `"en"`, `"fr"`). RSS-only.
    pub language: Option<String>,
    /// Most-recent update timestamp. Defaults to the latest
    /// `FeedItem::pub_date` at render time when `None`.
    pub last_build_date: Option<DateTime<Utc>>,
    pub items: Vec<FeedItem>,
}

/// One entry in a feed.
#[derive(Debug, Clone)]
pub struct FeedItem {
    pub title: String,
    /// Per-item URL — RSS `<link>` / `<guid>`, Atom `<id>`.
    pub link: String,
    pub description: Option<String>,
    pub pub_date: Option<DateTime<Utc>>,
    /// Free-form author identifier. RSS: an email address per spec;
    /// Atom: any string (rendered as `<author><name>...</name></author>`).
    pub author: Option<String>,
    /// Optional override for the per-item `<guid>` (RSS) / `<id>`
    /// (Atom). Defaults to `link` when unset.
    pub guid: Option<String>,
}

impl FeedItem {
    /// Build an item with the two required fields (title + link).
    #[must_use]
    pub fn new(title: impl Into<String>, link: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            link: link.into(),
            description: None,
            pub_date: None,
            author: None,
            guid: None,
        }
    }

    #[must_use]
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    #[must_use]
    pub fn with_pub_date(mut self, when: DateTime<Utc>) -> Self {
        self.pub_date = Some(when);
        self
    }

    #[must_use]
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    #[must_use]
    pub fn with_guid(mut self, guid: impl Into<String>) -> Self {
        self.guid = Some(guid.into());
        self
    }
}

// ------------------------------------------------------------------ render_rss

/// Render an RSS 2.0 document. Per spec: `<rss version="2.0">` with a
/// single `<channel>` containing channel metadata + `<item>` children.
#[must_use]
pub fn render_rss(feed: &Feed) -> String {
    let mut out = String::with_capacity(512 + feed.items.len() * 256);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<rss version=\"2.0\">\n");
    out.push_str("  <channel>\n");
    push_text_element(&mut out, "title", &feed.title, 4);
    push_text_element(&mut out, "link", &feed.link, 4);
    push_text_element(&mut out, "description", &feed.description, 4);
    if let Some(lang) = &feed.language {
        push_text_element(&mut out, "language", lang, 4);
    }
    let last_build = feed
        .last_build_date
        .or_else(|| feed.items.iter().filter_map(|i| i.pub_date).max());
    if let Some(ts) = last_build {
        push_text_element(&mut out, "lastBuildDate", &format_rfc822(ts), 4);
    }
    for item in &feed.items {
        out.push_str("    <item>\n");
        push_text_element(&mut out, "title", &item.title, 6);
        push_text_element(&mut out, "link", &item.link, 6);
        if let Some(desc) = &item.description {
            push_text_element(&mut out, "description", desc, 6);
        }
        if let Some(author) = &item.author {
            push_text_element(&mut out, "author", author, 6);
        }
        if let Some(ts) = item.pub_date {
            push_text_element(&mut out, "pubDate", &format_rfc822(ts), 6);
        }
        let guid = item.guid.as_deref().unwrap_or(&item.link);
        push_text_element(&mut out, "guid", guid, 6);
        out.push_str("    </item>\n");
    }
    out.push_str("  </channel>\n");
    out.push_str("</rss>\n");
    out
}

// ------------------------------------------------------------------ render_atom

/// Render an Atom 1.0 document. The Atom spec mandates `<id>` /
/// `<title>` / `<updated>` on both the feed and each entry; we fill
/// in `feed.link` for `<id>`, the max `pub_date` (or `Utc::now()`)
/// for `<updated>` when not set, and per-item `link`/`pub_date`
/// similarly.
#[must_use]
pub fn render_atom(feed: &Feed) -> String {
    let mut out = String::with_capacity(512 + feed.items.len() * 256);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
    push_text_element(&mut out, "title", &feed.title, 2);
    push_text_element(&mut out, "id", &feed.link, 2);
    // `<link rel="alternate" href="..."/>` per Atom convention.
    out.push_str("  <link href=\"");
    let mut esc_link = String::new();
    escape_xml_text(&mut esc_link, &feed.link);
    out.push_str(&esc_link);
    out.push_str("\" rel=\"alternate\"/>\n");
    push_text_element(&mut out, "subtitle", &feed.description, 2);
    let updated = feed
        .last_build_date
        .or_else(|| feed.items.iter().filter_map(|i| i.pub_date).max())
        .unwrap_or_else(Utc::now);
    push_text_element(&mut out, "updated", &format_iso8601(updated), 2);
    for item in &feed.items {
        out.push_str("  <entry>\n");
        push_text_element(&mut out, "title", &item.title, 4);
        let id = item.guid.as_deref().unwrap_or(&item.link);
        push_text_element(&mut out, "id", id, 4);
        out.push_str("    <link href=\"");
        let mut esc = String::new();
        escape_xml_text(&mut esc, &item.link);
        out.push_str(&esc);
        out.push_str("\" rel=\"alternate\"/>\n");
        if let Some(desc) = &item.description {
            push_text_element(&mut out, "summary", desc, 4);
        }
        if let Some(author) = &item.author {
            out.push_str("    <author>\n");
            push_text_element(&mut out, "name", author, 6);
            out.push_str("    </author>\n");
        }
        let entry_updated = item.pub_date.unwrap_or(updated);
        push_text_element(&mut out, "updated", &format_iso8601(entry_updated), 4);
        out.push_str("  </entry>\n");
    }
    out.push_str("</feed>\n");
    out
}

// ------------------------------------------------------------------ helpers

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

/// RFC 822 (= RFC 2822) date format required by RSS 2.0 `<pubDate>`
/// / `<lastBuildDate>`. Chrono provides this directly.
fn format_rfc822(ts: DateTime<Utc>) -> String {
    ts.to_rfc2822()
}

/// ISO-8601 / RFC 3339 — Atom 1.0 `<updated>` shape. UTC zero offset
/// renders as `Z`.
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

    fn sample_feed() -> Feed {
        Feed {
            title: "Example".into(),
            link: "https://example.com/".into(),
            description: "Test feed".into(),
            language: Some("en".into()),
            last_build_date: None,
            items: vec![
                FeedItem::new("First post", "https://example.com/articles/first")
                    .with_description("body 1")
                    .with_pub_date(ts(2025, 1, 1))
                    .with_author("alice@example.com"),
                FeedItem::new("Second post", "https://example.com/articles/second")
                    .with_pub_date(ts(2025, 3, 15)),
            ],
        }
    }

    #[test]
    fn item_builder_chains() {
        let i = FeedItem::new("t", "https://x/")
            .with_description("d")
            .with_pub_date(ts(2025, 1, 1))
            .with_author("a")
            .with_guid("g");
        assert_eq!(i.title, "t");
        assert_eq!(i.description.as_deref(), Some("d"));
        assert!(i.pub_date.is_some());
        assert_eq!(i.author.as_deref(), Some("a"));
        assert_eq!(i.guid.as_deref(), Some("g"));
    }

    // ---------- RSS ----------

    #[test]
    fn rss_has_declaration_and_channel() {
        let xml = render_rss(&sample_feed());
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(xml.contains("<rss version=\"2.0\">"));
        assert!(xml.contains("<channel>"));
        assert!(xml.contains("<title>Example</title>"));
        assert!(xml.contains("<link>https://example.com/</link>"));
        assert!(xml.contains("<description>Test feed</description>"));
        assert!(xml.contains("<language>en</language>"));
        assert!(xml.ends_with("</rss>\n"));
    }

    #[test]
    fn rss_emits_all_items_in_order() {
        let xml = render_rss(&sample_feed());
        let first_idx = xml.find("First post").unwrap();
        let second_idx = xml.find("Second post").unwrap();
        assert!(first_idx < second_idx, "items should render in input order");
    }

    #[test]
    fn rss_uses_link_as_default_guid() {
        let xml = render_rss(&sample_feed());
        assert!(xml.contains("<guid>https://example.com/articles/first</guid>"));
    }

    #[test]
    fn rss_uses_explicit_guid_when_set() {
        let mut f = sample_feed();
        f.items[0] = FeedItem::new("Pinned", "https://example.com/articles/pinned")
            .with_guid("urn:my-app:pinned-42");
        let xml = render_rss(&f);
        assert!(xml.contains("<guid>urn:my-app:pinned-42</guid>"));
    }

    #[test]
    fn rss_last_build_date_defaults_to_max_pub_date() {
        let xml = render_rss(&sample_feed());
        // sample items: 2025-01-01 and 2025-03-15 → max is 2025-03-15.
        let expected = format_rfc822(ts(2025, 3, 15));
        assert!(
            xml.contains(&format!("<lastBuildDate>{expected}</lastBuildDate>")),
            "lastBuildDate should default to latest pub_date; got: {xml}"
        );
    }

    #[test]
    fn rss_emits_pubdate_in_rfc822() {
        let xml = render_rss(&sample_feed());
        let expected = format_rfc822(ts(2025, 1, 1));
        assert!(
            xml.contains(&format!("<pubDate>{expected}</pubDate>")),
            "pubDate not in RFC 822 shape: {xml}"
        );
    }

    #[test]
    fn rss_omits_optional_fields_when_unset() {
        let f = Feed {
            title: "Minimal".into(),
            link: "https://x/".into(),
            description: "d".into(),
            language: None,
            last_build_date: None,
            items: vec![FeedItem::new("a", "https://x/a")],
        };
        let xml = render_rss(&f);
        assert!(!xml.contains("<language>"));
        assert!(!xml.contains("<lastBuildDate>"));
        // Channel <description> is REQUIRED by RSS 2.0 — it stays.
        // Items without their own description should still omit the
        // tag from the <item> block; assert only the channel one
        // appears (count == 1).
        assert_eq!(
            xml.matches("<description>").count(),
            1,
            "channel description should appear once, item description omitted: {xml}"
        );
        assert!(!xml.contains("<author>"));
        assert!(!xml.contains("<pubDate>"));
    }

    #[test]
    fn rss_escapes_xml_special_chars() {
        let f = Feed {
            title: "Q&A <special>".into(),
            link: "https://example.com/?q=rust&lang=en".into(),
            description: "X > Y < Z".into(),
            language: None,
            last_build_date: None,
            items: vec![],
        };
        let xml = render_rss(&f);
        assert!(xml.contains("<title>Q&amp;A &lt;special&gt;</title>"));
        assert!(xml.contains("https://example.com/?q=rust&amp;lang=en"));
        assert!(xml.contains("<description>X &gt; Y &lt; Z</description>"));
    }

    // ---------- Atom ----------

    #[test]
    fn atom_has_declaration_and_feed_namespace() {
        let xml = render_atom(&sample_feed());
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));
        assert!(xml.contains("<title>Example</title>"));
        assert!(xml.contains("<id>https://example.com/</id>"));
        assert!(xml.contains("<link href=\"https://example.com/\" rel=\"alternate\"/>"));
        assert!(xml.ends_with("</feed>\n"));
    }

    #[test]
    fn atom_renders_entries() {
        let xml = render_atom(&sample_feed());
        // Each item → <entry> with title, id (= link by default), link.
        assert!(xml.contains("<entry>"));
        assert!(xml.contains("<title>First post</title>"));
        assert!(xml.contains("<id>https://example.com/articles/first</id>"));
        assert!(
            xml.contains("<link href=\"https://example.com/articles/first\" rel=\"alternate\"/>")
        );
    }

    #[test]
    fn atom_emits_author_as_nested_name() {
        let xml = render_atom(&sample_feed());
        assert!(
            xml.contains("<author>\n      <name>alice@example.com</name>\n    </author>"),
            "Atom author should render as <author><name>...</name></author>: {xml}"
        );
    }

    #[test]
    fn atom_updated_falls_back_to_max_pub_date() {
        let xml = render_atom(&sample_feed());
        let expected = format_iso8601(ts(2025, 3, 15));
        assert!(
            xml.contains(&format!("<updated>{expected}</updated>")),
            "feed-level <updated> should default to latest item pub_date: {xml}"
        );
    }

    #[test]
    fn atom_entry_updated_falls_back_to_feed_updated() {
        let mut f = sample_feed();
        // Strip pub_dates so we exercise the fallback path.
        for it in &mut f.items {
            it.pub_date = None;
        }
        f.last_build_date = Some(ts(2025, 6, 1));
        let xml = render_atom(&f);
        let expected = format_iso8601(ts(2025, 6, 1));
        // Both the feed `<updated>` and every entry's `<updated>`
        // should carry this timestamp.
        let count = xml
            .matches(&format!("<updated>{expected}</updated>"))
            .count();
        assert!(
            count >= 3,
            "expected feed + 2 entries to share updated; got {count}: {xml}"
        );
    }
}
