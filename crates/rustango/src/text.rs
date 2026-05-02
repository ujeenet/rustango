//! Text utilities — slug generation, HTML escaping, truncation.
//!
//! Small zero-dep helpers for the common bits of text-handling boilerplate
//! every web app needs.

// ------------------------------------------------------------------ slugify

/// Convert a string into a URL-safe slug.
///
/// - Lowercases ASCII letters
/// - Replaces non-alphanumeric runs with a single `-`
/// - Strips leading and trailing `-`
/// - Drops non-ASCII characters (use [`slugify_unicode`] for transliteration support)
///
/// # Examples
///
/// ```
/// use rustango::text::slugify;
/// assert_eq!(slugify("Hello, World!"), "hello-world");
/// assert_eq!(slugify("Rust  &  Django"), "rust-django");
/// assert_eq!(slugify("  --leading--  "), "leading");
/// ```
#[must_use]
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !out.is_empty() && !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    trimmed.to_owned()
}

/// Like [`slugify`] but preserves Unicode letters/digits (lowercased).
/// Useful when your URL infrastructure handles UTF-8 paths.
#[must_use]
pub fn slugify_unicode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lower in c.to_lowercase() {
                out.push(lower);
            }
            last_was_dash = false;
        } else if !out.is_empty() && !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

// ------------------------------------------------------------------ HTML escape

/// Escape a string for safe insertion into HTML element content or
/// double-quoted HTML attributes.
///
/// Replaces `&`, `<`, `>`, `"`, `'` with their HTML entities.
///
/// # Example
///
/// ```
/// use rustango::text::html_escape;
/// assert_eq!(html_escape("<script>"), "&lt;script&gt;");
/// assert_eq!(html_escape("a & b"), "a &amp; b");
/// ```
#[must_use]
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

// ------------------------------------------------------------------ truncate

/// Truncate `s` to at most `max_chars` characters. If truncation happens,
/// append `suffix` (typically `"…"` or `"..."`).
///
/// Counts CHARACTERS, not bytes — never breaks UTF-8 boundaries.
///
/// # Example
///
/// ```
/// use rustango::text::truncate;
/// assert_eq!(truncate("hello world", 5, "…"), "hello…");
/// assert_eq!(truncate("short", 10, "…"), "short");
/// ```
#[must_use]
pub fn truncate(s: &str, max_chars: usize, suffix: &str) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }

    #[test]
    fn slugify_strips_punctuation() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("Rust & Django"), "rust-django");
    }

    #[test]
    fn slugify_collapses_whitespace_runs() {
        assert_eq!(slugify("foo    bar"), "foo-bar");
        assert_eq!(slugify("foo--bar"), "foo-bar");
    }

    #[test]
    fn slugify_trims_dashes() {
        assert_eq!(slugify("---foo---"), "foo");
        assert_eq!(slugify("  hi  "), "hi");
    }

    #[test]
    fn slugify_drops_non_ascii() {
        assert_eq!(slugify("Café"), "caf");
        assert_eq!(slugify("日本語"), "");
    }

    #[test]
    fn slugify_empty_input() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("   "), "");
    }

    #[test]
    fn slugify_unicode_keeps_letters() {
        assert_eq!(slugify_unicode("Café"), "café");
        assert_eq!(slugify_unicode("Hello, 世界!"), "hello-世界");
    }

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("<a>&\"'</a>"), "&lt;a&gt;&amp;&quot;&#x27;&lt;/a&gt;");
    }

    #[test]
    fn html_escape_passes_safe_chars() {
        assert_eq!(html_escape("hello world 123"), "hello world 123");
    }

    #[test]
    fn html_escape_xss_attack_examples() {
        // Common XSS attempts — after escape, none should produce executable HTML
        let evil = r#"<script>alert("xss")</script>"#;
        let safe = html_escape(evil);
        assert!(!safe.contains("<script>"));
        assert!(!safe.contains("</script>"));
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("hi", 10, "…"), "hi");
    }

    #[test]
    fn truncate_long_appends_suffix() {
        assert_eq!(truncate("hello world", 5, "…"), "hello…");
        assert_eq!(truncate("hello world", 5, "..."), "hello...");
    }

    #[test]
    fn truncate_at_exact_boundary_unchanged() {
        assert_eq!(truncate("hello", 5, "…"), "hello");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        // "café" is 4 chars but 5 bytes in UTF-8 — must respect char boundary
        assert_eq!(truncate("café au lait", 4, "…"), "café…");
    }
}
