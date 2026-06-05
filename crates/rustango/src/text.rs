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

/// Generate a unique slug by appending `-2`, `-3`, ... until `is_taken`
/// returns false. Useful for URL slugs where the natural `slugify(title)`
/// might collide with an existing row.
///
/// `is_taken` is a closure called once per candidate; it should return
/// `true` if the slug already exists in your DB.
///
/// # Examples
///
/// ```
/// use rustango::text::unique_slug;
///
/// let mut existing = std::collections::HashSet::new();
/// existing.insert("hello-world".to_owned());
/// existing.insert("hello-world-2".to_owned());
///
/// let slug = unique_slug("Hello, World!", |s| existing.contains(s));
/// assert_eq!(slug, "hello-world-3");
/// ```
///
/// For DB-backed checks, wrap your async lookup:
///
/// ```ignore
/// let slug = unique_slug_async(&title, |candidate| async {
///     Post::objects().where_(Post::slug.eq(candidate.to_owned())).count(&pool).await? > 0
/// }).await?;
/// ```
#[must_use]
pub fn unique_slug<F>(input: &str, mut is_taken: F) -> String
where
    F: FnMut(&str) -> bool,
{
    let base = slugify(input);
    if !is_taken(&base) {
        return base;
    }
    for i in 2..u32::MAX {
        let candidate = format!("{base}-{i}");
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    base // pathological — fall back to base
}

/// Async variant of [`unique_slug`] for DB-backed uniqueness checks.
pub async fn unique_slug_async<F, Fut>(input: &str, mut is_taken: F) -> String
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let base = slugify(input);
    if !is_taken(base.clone()).await {
        return base;
    }
    for i in 2..u32::MAX {
        let candidate = format!("{base}-{i}");
        if !is_taken(candidate.clone()).await {
            return candidate;
        }
    }
    base
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

/// Django-parity `Truncator(s).words(num, truncate=…)` — truncate
/// to the first `max_words` whitespace-separated tokens, appending
/// `suffix` when truncation actually fired.
///
/// Whitespace runs collapse to a single space in the output (Django
/// keeps single spaces between preserved words — leading and
/// trailing whitespace is trimmed in the truncated form).
///
/// ```
/// use rustango::text::truncate_words;
/// assert_eq!(truncate_words("Joel is a slug", 2, " …"), "Joel is …");
/// assert_eq!(truncate_words("short text", 5, "…"), "short text");
/// assert_eq!(truncate_words("", 5, "…"), "");
/// ```
#[must_use]
pub fn truncate_words(s: &str, max_words: usize, suffix: &str) -> String {
    let mut iter = s.split_whitespace();
    let mut kept: Vec<&str> = Vec::with_capacity(max_words);
    for _ in 0..max_words {
        if let Some(w) = iter.next() {
            kept.push(w);
        } else {
            break;
        }
    }
    // If there's anything left in the iterator, truncation fired.
    let truncated = iter.next().is_some();
    let mut out = kept.join(" ");
    if truncated {
        out.push_str(suffix);
    }
    // No truncation + original had no internal whitespace collapse?
    // We still return the joined-by-single-space form to match
    // Django shape. Callers wanting verbatim text shouldn't pass it
    // through truncate_words at all.
    out
}

/// Django-parity `django.utils.text.normalize_newlines(text)` —
/// convert all `\r\n` / `\r` sequences to plain `\n`. Useful when
/// processing `<textarea>` form input, where browsers historically
/// submit CRLF line endings regardless of the originating platform.
///
/// ```
/// use rustango::text::normalize_newlines;
/// assert_eq!(normalize_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
/// assert_eq!(normalize_newlines(""), "");
/// ```
#[must_use]
pub fn normalize_newlines(s: &str) -> String {
    // Two passes is simpler than a state machine and only marginally
    // slower; CRLF first so the second pass doesn't double-translate.
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Django-parity `django.utils.text.capfirst(x)` — capitalize the
/// first character of `s`, leaving the rest untouched. Distinct
/// from `str::to_title_case` / Python `.title()` which would
/// capitalize every word.
///
/// Empty input returns the empty string; the first non-ASCII
/// character is uppercased via Unicode case-mapping (`char::to_uppercase`)
/// so multi-codepoint expansions (e.g. `ß` → `SS`) work right.
///
/// ```
/// use rustango::text::capfirst;
/// assert_eq!(capfirst("hello world"), "Hello world");
/// assert_eq!(capfirst("Hello"), "Hello");
/// assert_eq!(capfirst(""), "");
/// assert_eq!(capfirst("ßomething"), "SSomething"); // Unicode upper expands
/// ```
#[must_use]
pub fn capfirst(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Django-parity `django.utils.text.get_text_list(list_, last_word='or')` —
/// join `items` into a comma-separated grammatical list with
/// `last_word` (typically `"or"` or `"and"`) as the conjunction
/// before the final element.
///
/// Examples (matching Django output exactly):
///
/// * `[]` → `""`
/// * `["a"]` → `"a"`
/// * `["a", "b"]` → `"a or b"` (no Oxford comma on two items)
/// * `["a", "b", "c"]` → `"a, b or c"`
/// * `["a", "b", "c", "d"]` → `"a, b, c or d"`
///
/// Note Django's `get_text_list` does NOT emit a serial-comma
/// before the conjunction; the existing Tera filter `oxford_join`
/// does (that's the Oxford-comma style). Use this when the Django
/// shape is the goal; use `oxford_join` for serial-comma style.
///
/// ```
/// use rustango::text::get_text_list;
/// assert_eq!(get_text_list(&["a", "b", "c"], "or"), "a, b or c");
/// assert_eq!(get_text_list(&["a", "b"], "and"), "a and b");
/// assert_eq!(get_text_list(&["only"], "or"), "only");
/// assert_eq!(get_text_list::<&str>(&[], "or"), "");
/// ```
#[must_use]
pub fn get_text_list<S: AsRef<str>>(items: &[S], last_word: &str) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].as_ref().to_owned(),
        2 => format!("{} {} {}", items[0].as_ref(), last_word, items[1].as_ref()),
        n => {
            let head = items[..n - 1]
                .iter()
                .map(|s| s.as_ref())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{head} {last_word} {}", items[n - 1].as_ref())
        }
    }
}

/// Django-parity `django.utils.text.smart_split(text)` — split
/// `text` on whitespace, honoring double-quoted substrings as
/// single tokens. Used by Django's admin search query parser.
///
/// Quotes themselves are KEPT in the output token (Django shape) —
/// strip them at the call site if you want bare strings. Backslash
/// escapes are preserved verbatim (`\"` inside a quoted string is
/// kept literal — Django does not unescape).
///
/// ```
/// use rustango::text::smart_split;
/// let tokens = smart_split(r#"This is "a test""#);
/// assert_eq!(tokens, vec!["This", "is", "\"a test\""]);
///
/// // Unmatched closing quote → kept as part of the trailing token.
/// let tokens = smart_split(r#"oops "no close"#);
/// assert_eq!(tokens, vec!["oops", "\"no close"]);
/// ```
#[must_use]
pub fn smart_split(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in text.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            current.push(c);
        } else if c.is_whitespace() && !in_quotes {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Django-parity
/// [`django.utils.html.strip_tags(value)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.strip_tags) —
/// remove HTML / XML tag markup from `s` and return the bare text
/// content.
///
/// Strips anything inside `< … >` pairs, including:
///
/// * Regular tags (`<p>foo</p>` → `foo`)
/// * Self-closing tags (`<br/>` → ``)
/// * Comments (`<!-- secret --> visible` → ` visible`)
/// * CDATA-style braces (`<![CDATA[…]]>` → ``)
///
/// **NOT a sanitizer.** Django's docstring explicitly warns the
/// same: this is for plain-text extraction (search indexing,
/// `Last-Modified` body preview, etc.). For user-input HTML
/// sanitization use an actual HTML parser + allowlist.
///
/// Empty `<>` (no tag name) is also stripped. Unclosed `<` at the
/// end of input is kept literal (Django shape — Python's regex
/// re-tries the trailing `<`).
///
/// ```ignore
/// use rustango::text::strip_tags;
/// assert_eq!(strip_tags("<p>hello <b>world</b></p>"), "hello world");
/// assert_eq!(strip_tags("plain text"), "plain text");
/// assert_eq!(strip_tags("a < b"), "a < b"); // unclosed `<` kept
/// ```
#[must_use]
pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut tag_start_byte: Option<usize> = None;
    // Iterate by char (NOT byte) so multi-byte UTF-8 codepoints
    // survive intact in the output.
    for (i, c) in s.char_indices() {
        if !in_tag {
            if c == '<' {
                in_tag = true;
                tag_start_byte = Some(i);
            } else {
                out.push(c);
            }
        } else if c == '>' {
            in_tag = false;
            tag_start_byte = None;
        }
    }
    // Unclosed `<` at end of input — push the trailing slice literally.
    if let Some(start) = tag_start_byte {
        out.push_str(&s[start..]);
    }
    out
}

/// Django-parity `phone2numeric` — convert phone-keypad letters to
/// the matching digit per ITU E.161 (`abc→2`, `def→3`, …, `wxyz→9`).
/// Case-insensitive; non-letters pass through unchanged.
///
/// The Tera filter [`crate::default_filters`] registers this same
/// transformation as the `|phone2numeric` template filter; this
/// free function lets handler code reach the same logic without
/// going through a `Value` round-trip.
///
/// ```
/// use rustango::text::phone2numeric;
/// assert_eq!(phone2numeric("1-800-COLLECT"), "1-800-2655328");
/// assert_eq!(phone2numeric("abcDEF"), "222333");
/// assert_eq!(phone2numeric("(555) 867-5309"), "(555) 867-5309");
/// ```
#[must_use]
pub fn phone2numeric(s: &str) -> String {
    s.chars()
        .map(|c| match c.to_ascii_lowercase() {
            'a' | 'b' | 'c' => '2',
            'd' | 'e' | 'f' => '3',
            'g' | 'h' | 'i' => '4',
            'j' | 'k' | 'l' => '5',
            'm' | 'n' | 'o' => '6',
            'p' | 'q' | 'r' | 's' => '7',
            't' | 'u' | 'v' => '8',
            'w' | 'x' | 'y' | 'z' => '9',
            _ => c,
        })
        .collect()
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
        assert_eq!(
            html_escape("<a>&\"'</a>"),
            "&lt;a&gt;&amp;&quot;&#x27;&lt;/a&gt;"
        );
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

    // -------------------------------------------------------------- unique_slug

    #[test]
    fn unique_slug_returns_base_when_free() {
        let result = unique_slug("Hello World", |_| false);
        assert_eq!(result, "hello-world");
    }

    #[test]
    fn unique_slug_appends_2_when_base_taken() {
        let mut existing = std::collections::HashSet::new();
        existing.insert("hello-world".to_owned());
        let result = unique_slug("Hello World", |s| existing.contains(s));
        assert_eq!(result, "hello-world-2");
    }

    #[test]
    fn unique_slug_keeps_incrementing_until_free() {
        let mut existing = std::collections::HashSet::new();
        for i in 1..=5 {
            let s = if i == 1 {
                "hello".to_owned()
            } else {
                format!("hello-{i}")
            };
            existing.insert(s);
        }
        let result = unique_slug("Hello", |s| existing.contains(s));
        assert_eq!(result, "hello-6");
    }

    #[tokio::test]
    async fn unique_slug_async_works() {
        let mut existing = std::collections::HashSet::new();
        existing.insert("foo".to_owned());
        existing.insert("foo-2".to_owned());

        let result = unique_slug_async("foo", |candidate| {
            let existing = existing.clone();
            async move { existing.contains(&candidate) }
        })
        .await;
        assert_eq!(result, "foo-3");
    }

    // -------- truncate_words (Django parity) --------

    #[test]
    fn truncate_words_basic() {
        assert_eq!(truncate_words("Joel is a slug", 2, " …"), "Joel is …");
    }

    #[test]
    fn truncate_words_under_limit_passes_through_collapsed() {
        // Django returns the input as-is (single-spaced), no suffix.
        assert_eq!(truncate_words("short text", 5, "…"), "short text");
    }

    #[test]
    fn truncate_words_at_exact_limit_no_suffix() {
        assert_eq!(truncate_words("one two three", 3, "…"), "one two three");
    }

    #[test]
    fn truncate_words_empty_input() {
        assert_eq!(truncate_words("", 5, "…"), "");
    }

    #[test]
    fn truncate_words_zero_limit() {
        // Zero words requested → no words kept; if the original had any
        // content, suffix appended.
        assert_eq!(truncate_words("anything", 0, "…"), "…");
        assert_eq!(truncate_words("", 0, "…"), "");
    }

    #[test]
    fn truncate_words_collapses_whitespace_runs() {
        // Django shape: kept words joined by single space regardless of
        // original whitespace shape.
        assert_eq!(truncate_words("a   b\t\tc\nd", 3, "…"), "a b c…");
    }

    // -------- normalize_newlines (Django parity) --------

    #[test]
    fn normalize_newlines_crlf_to_lf() {
        assert_eq!(normalize_newlines("a\r\nb"), "a\nb");
    }

    #[test]
    fn normalize_newlines_lone_cr_to_lf() {
        // Classic-Mac line ending — convert to LF.
        assert_eq!(normalize_newlines("a\rb"), "a\nb");
    }

    #[test]
    fn normalize_newlines_mixed() {
        // CRLF + lone CR + LF + bare text.
        assert_eq!(normalize_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn normalize_newlines_already_lf_passes_through() {
        assert_eq!(normalize_newlines("a\nb\nc"), "a\nb\nc");
    }

    #[test]
    fn normalize_newlines_empty() {
        assert_eq!(normalize_newlines(""), "");
    }

    #[test]
    fn normalize_newlines_no_newlines_at_all() {
        assert_eq!(normalize_newlines("plain text"), "plain text");
    }

    // -------- phone2numeric (Django parity) --------

    #[test]
    fn phone2numeric_canonical() {
        assert_eq!(phone2numeric("1-800-COLLECT"), "1-800-2655328");
    }

    #[test]
    fn phone2numeric_case_insensitive() {
        assert_eq!(phone2numeric("abcDEF"), "222333");
    }

    #[test]
    fn phone2numeric_passes_non_letters() {
        assert_eq!(phone2numeric("(555) 867-5309"), "(555) 867-5309");
    }

    #[test]
    fn phone2numeric_all_letter_groups() {
        // Coverage check — every keypad group maps right.
        assert_eq!(phone2numeric("abc"), "222");
        assert_eq!(phone2numeric("def"), "333");
        assert_eq!(phone2numeric("ghi"), "444");
        assert_eq!(phone2numeric("jkl"), "555");
        assert_eq!(phone2numeric("mno"), "666");
        assert_eq!(phone2numeric("pqrs"), "7777");
        assert_eq!(phone2numeric("tuv"), "888");
        assert_eq!(phone2numeric("wxyz"), "9999");
    }

    #[test]
    fn phone2numeric_empty() {
        assert_eq!(phone2numeric(""), "");
    }

    // -------- capfirst (Django parity) --------

    #[test]
    fn capfirst_simple() {
        assert_eq!(capfirst("hello world"), "Hello world");
    }

    #[test]
    fn capfirst_already_capitalized() {
        assert_eq!(capfirst("Hello"), "Hello");
    }

    #[test]
    fn capfirst_empty_is_empty() {
        assert_eq!(capfirst(""), "");
    }

    #[test]
    fn capfirst_single_char() {
        assert_eq!(capfirst("a"), "A");
    }

    #[test]
    fn capfirst_unicode_expanding_case() {
        // German sharp s uppercases to two chars (SS) per Unicode rules.
        // Django's `.capitalize()` would also expand; we follow.
        assert_eq!(capfirst("ßomething"), "SSomething");
    }

    #[test]
    fn capfirst_does_not_touch_rest() {
        // Django capfirst doesn't lowercase the tail (distinct from
        // Python's `.capitalize()` which DOES). Match Django.
        assert_eq!(capfirst("hELLO"), "HELLO");
    }

    // -------- get_text_list (Django parity) --------

    #[test]
    fn get_text_list_empty() {
        assert_eq!(get_text_list::<&str>(&[], "or"), "");
    }

    #[test]
    fn get_text_list_single() {
        assert_eq!(get_text_list(&["only"], "or"), "only");
    }

    #[test]
    fn get_text_list_two_uses_conjunction_only() {
        // Two items → "a or b" (no comma).
        assert_eq!(get_text_list(&["a", "b"], "or"), "a or b");
        assert_eq!(get_text_list(&["a", "b"], "and"), "a and b");
    }

    #[test]
    fn get_text_list_three_uses_no_serial_comma() {
        // Django shape: "a, b or c" — no Oxford comma.
        assert_eq!(get_text_list(&["a", "b", "c"], "or"), "a, b or c");
    }

    #[test]
    fn get_text_list_many() {
        assert_eq!(get_text_list(&["a", "b", "c", "d"], "or"), "a, b, c or d");
    }

    #[test]
    fn get_text_list_with_string_owned() {
        // Works with `String` too via AsRef<str>.
        let items: Vec<String> = vec!["one".into(), "two".into(), "three".into()];
        assert_eq!(get_text_list(&items, "or"), "one, two or three");
    }

    // -------- smart_split (Django parity) --------

    #[test]
    fn smart_split_simple_whitespace() {
        assert_eq!(
            smart_split("This is a test"),
            vec!["This", "is", "a", "test"]
        );
    }

    #[test]
    fn smart_split_preserves_quoted_substrings() {
        let got = smart_split(r#"This is "a test""#);
        assert_eq!(got, vec!["This", "is", r#""a test""#]);
    }

    #[test]
    fn smart_split_multiple_quoted_groups() {
        let got = smart_split(r#""one two" "three four""#);
        assert_eq!(got, vec![r#""one two""#, r#""three four""#]);
    }

    #[test]
    fn smart_split_unmatched_quote_keeps_trailing_token() {
        // Django shape: unmatched closing quote does NOT panic; the
        // unfinished quoted span is kept as one trailing token.
        let got = smart_split(r#"oops "no close"#);
        assert_eq!(got, vec!["oops", r#""no close"#]);
    }

    #[test]
    fn smart_split_empty_string() {
        let got = smart_split("");
        assert!(got.is_empty());
    }

    #[test]
    fn smart_split_whitespace_only_returns_empty() {
        let got = smart_split("   \t  \n  ");
        assert!(got.is_empty());
    }

    #[test]
    fn smart_split_collapses_consecutive_whitespace() {
        let got = smart_split("a    b\t\tc");
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    // -------- strip_tags (Django parity) --------

    #[test]
    fn strip_tags_removes_basic_tags() {
        assert_eq!(strip_tags("<p>hello</p>"), "hello");
        assert_eq!(strip_tags("<p>hello <b>world</b></p>"), "hello world");
    }

    #[test]
    fn strip_tags_handles_self_closing() {
        assert_eq!(strip_tags("line<br/>break"), "linebreak");
        assert_eq!(strip_tags("<img src=\"x\" />after"), "after");
    }

    #[test]
    fn strip_tags_strips_comments() {
        assert_eq!(strip_tags("<!-- secret -->visible"), "visible");
    }

    #[test]
    fn strip_tags_passes_through_text_without_tags() {
        assert_eq!(strip_tags("plain text"), "plain text");
    }

    #[test]
    fn strip_tags_keeps_unclosed_trailing_lt() {
        // Django regex skips an unmatched trailing `<` rather than
        // eating to end-of-input.
        assert_eq!(strip_tags("a < b"), "a < b");
    }

    #[test]
    fn strip_tags_empty() {
        assert_eq!(strip_tags(""), "");
        // Empty tag content also strips cleanly.
        assert_eq!(strip_tags("<>"), "");
    }

    #[test]
    fn strip_tags_handles_nested_quotes_in_attrs() {
        // The naive parser doesn't track quote balance — `<a href=">"`
        // closes on the first `>`. This matches Django's regex
        // behavior, which also can't track quoted attrs. The fact
        // that we're consistent with Django is the point.
        assert_eq!(strip_tags(r#"<a href="x">link</a>"#), "link");
    }

    #[test]
    fn strip_tags_preserves_unicode_content() {
        assert_eq!(strip_tags("<p>café — résumé</p>"), "café — résumé");
    }
}
